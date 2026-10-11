# Tuner — Design (v1)

> **Status: authoritative (revision 2, after the UX / architecture / work-plan reviews).**
> This document, the public signatures in `src/`, the keymap tables in `src/keymap.rs` and the
> schema in `src/store/migrations/0001_init.sql` define v1. Where this text and the code
> disagree on a *signature*, the code wins and this text is fixed; where they disagree on
> *behaviour*, this text wins and the code is fixed.
>
> Inputs: `research/uhf.md` (feature inventory), `research/omarchy.md` (design tokens,
> integration), `research/protocols.md` (formats, crates), `research/spike.md` (proven
> egui+libmpv recipe), `research/critique.md` (corrections, gaps, risks — all addressed in §2).
> Review round 2 (UX, architecture, implementability): every accepted BLOCKER/MAJOR is folded
> into the sections below; rejected or modified points are listed in §26.

Contents: 1 Product & scope · 2 Decisions & critique resolutions · 3 Architecture, threads,
data flow · 4 Module map · 5 Runtime details · 6 Data model · 7 Storage lifecycle · 8 Identity &
reattach · 9 Import pipelines & EPG matching · 10 Catch-up · 11 Player · 12 Layout ·
13 Screens & components · 14 Search · 15 Keyboard · 16 Theme engine · 17 Fonts · 18 Images ·
19 Backup · 20 Security · 21 Desktop integration · 22 Packaging & licence · 23 Testing ·
24 Implementation plan (work packages) · 25 Risks & verification backlog · 26 Review decisions

---

## 1. Product overview & v1 scope

**Tuner** is a native IPTV player for Omarchy (Hyprland). It targets the feature set users love
in UHF (Apple-only) — fast live TV with a real guide, catch-up, VOD with resume, global search,
smart reconnects — and fixes UHF's documented gaps (hidden channels, orderable EPG sources,
stale guides, configurable skip, subtitle delay, stream info, profiles-friendly storage). It
*looks and behaves like Omarchy*: keyboard-first (launcher idiom in lists, vim keys where no
text is typed), square corners, JetBrains Mono Nerd Font, colours taken live from the current
Omarchy theme, foreground-alpha state tints that work in light themes.

* Binary / Wayland `app_id` / Hyprland class / MPRIS suffix / `.desktop`: **`tuner`**.
* Stack: Rust 2024 · eframe/egui 0.36.2 (**glow** renderer; glow 0.17 via `eframe::glow` only) ·
  libmpv 0.41 through the render API into an FBO texture (spike-proven) · SQLite (rusqlite,
  bundled, WAL) · tokio · reqwest (rustls).
* Hardware reality (this machine): Apple M2 / Asahi, Mesa GL 4.6, **no HW video decode**
  (software ≈ 56 % of one core per 1080p50 stream), 2560×1600 @ 1.6 scale (≈ 1600 logical px
  wide; tiled half ≈ 790), 60 Hz, no VRR.

### 1.1 v1 scope

| Area | Feature | Prio (uhf.md) | v1 | Where |
|---|---|---|---|---|
| Sources | M3U by URL and local file | MUST | ✅ | `sources::m3u`, `jobs::import_source` |
| | Xtream Codes (live + VOD + series + provider EPG) | MUST | ✅ | `sources::xtream` |
| | Multiple playlists, colour + glyph per source, badges everywhere | MUST | ✅ | `model::Source`, `widgets::badge` |
| | Manual + automatic refresh, on-launch-if-stale | MUST | ✅ | `jobs::scheduler` §9.5 |
| | Paste `get.php?username=…` → offered as Xtream | — | ✅ | `m3u::detect_xtream_url` |
| | TS / M3U8 preference | SHOULD | ✅ | `SourceSettings.live_format` |
| | Account info (expiry, connections, formats) | SHOULD | ✅ | `AccountInfo`, Settings ▸ Playlists |
| | Smart name cleanup | SHOULD | ✅ | `sources::smart_names` |
| EPG | XMLTV URL/file (gz/xz), provider guides | MUST | ✅ | `sources::xmltv`, `jobs::import_epg` |
| | Ordered EPG sources (priority — keyboard + drag), per-source offset | MUST | ✅ | `epg_source.priority/offset_min`, Settings ▸ EPG |
| | tvg-id matching + manual override | MUST | ✅ | §9.4 |
| | Fuzzy match fallback (suggestions) + manual assignment screen | SHOULD | ✅ | `epg_match::suggest`, EPG mapping screen |
| | Background EPG refresh, "updated N h ago" | MUST | ✅ | §9.5 |
| Live | Groups, channel list with now/next + progress, preview | MUST | ✅ | Live screen §13.2 |
| | EPG grid, programme details | MUST | ✅ | Guide §13.4 |
| | Zapping within collection, last-channel recall, history | MUST | ✅ | `PlayerCommand::Zap/LastChannel`, `h` |
| | Favourites, **multiple lists** (create/rename/delete/reorder) | MUST/SHOULD | ✅ | `fav_list`, `fav_item`, Ctrl+Shift+F, Settings ▸ Favourites |
| | Hide categories **and channels**, rename, reorder | MUST/SHOULD | ✅ | `item_override`, `group_override` |
| | Context menu | MUST | ✅ | `overlays::dialogs::ContextMenuOverlay` |
| | Sort options (provider, number, A–Z, recently added; VOD: rating, year) | SHOULD | ✅ | `SortMode` |
| Catch-up | Xtream + M3U `default/append/shift/flussonic/xc/vod` | MUST | ✅ | §10 |
| | Per-channel catch-up offset | SHOULD | ✅ | `item_override.catchup_offset_s`, Ctrl+I Edit information |
| VOD | Movies & series, posters, Xtream metadata, seasons/episodes | MUST | ✅ | Movies/Series §13.5–13.6 |
| | Resume / Start over, Continue watching, watched markers | MUST | ✅ | `watch_progress` |
| Home | Continue watching, favourite channels now/next, recent channels, recently added | MUST | ✅ | Home §13.3 |
| Search | Global fuzzy search (`/`, Ctrl+K), sections, EPG programme search | MUST | ✅ | §14 |
| Player | Audio/subtitle selection + language prefs, configurable skip | MUST | ✅ | `PlayerCommand`, config |
| | Smart reconnect, fullscreen, idle inhibit | MUST | ✅ | §11.4, §21.2 |
| | Subtitle delay/size, stream info, aspect/zoom | SHOULD | ✅ | `I`, `z/x`, `{ }`, `v/V` |
| | Zapping delay, buffer settings ("instant load") | SHOULD | ✅ | §11.5, `PlayerConfig` |
| | Mini player (corner, same stream) | SHOULD | ✅ | `VideoHost::mini_player` |
| Integration | MPRIS (media keys, bar widget) | SHOULD | ✅ | §21.1 |
| | Reminders (desktop notification + Watch action; `r` in the guide; list + cancel) | SHOULD | ✅ | §21.3, Settings ▸ Reminders |
| | `.desktop`, icon, Hyprland rules, omarchy-menu, PKGBUILD | — | ✅ | `packaging/` §22 |
| Misc | Parental PIN | SHOULD | ✅ | §20.3 |
| | Export / import backup | SHOULD | ✅ | §19 |
| | Debug log viewer + export, `?` help overlay from keymap | MUST | ✅ | Logs screen, §15 |

**Out of v1** (design keeps the door open — hook in parentheses): multiview (connection budget
§11.5, `VideoSurface` is per-instance), local recording (mpv `stream-record`; budget lease),
live-buffer rewind + "Go Live" (`demuxer-max-back-bytes` already set; `seekable` is forced false
for live in v1; would bind `,`/`.`), Trakt/TMDB (VodSummary has `tmdb_id`), Plex/Jellyfin
(`SourceKind` is an enum; the importer is per kind), profiles (all state under XDG dirs;
`--profile-dir` exists), sync (backup format is the sync payload), pinned Home sections
(`group_override.pinned_home` exists, UI later), OpenSubtitles, casting, VPN, adaptive frame
rate, background refresh while closed (`tuner --refresh` flag reserved for a systemd timer).

---

## 2. Fixed decisions & critique resolutions

| # | Topic | Resolution |
|---|---|---|
| D1 | Rendering | Spike recipe in `player/render.rs` (implemented): own `mpv_render_context` via libmpv2-sys, `libmpv2::Mpv` for control, FBO+RGBA8 texture registered with `register_native_glow_texture`, `FLIP_Y=0`, no `report_swap`, `BLOCK_FOR_TARGET_TIME=0`, `video-timing-offset=0`. hwdec `"auto"` → `no` on Asahi (device-tree `apple,`), `auto-safe` elsewhere. |
| D2 | mpv events | Dedicated `mpv-events` thread blocks in raw `mpv_wait_event` and feeds a **controller thread** that owns the state machine; reconnects, zaps, MPRIS and reminders never depend on the UI running (critique risk 2). **UI code talks to mpv only through `PlayerHandle`** (non-blocking channel) and reads snapshots; it never calls `Mpv::*` and never waits on the controller (mpv `render.h`: the render thread — our UI thread — must not call non-render-safe libmpv functions). |
| D3 | One stream | One mpv instance; the video surface is drawn as Live preview, fullscreen, or mini-player. Connection budget + stop-before-start §11.5. |
| D4 | Storage | SQLite WAL at `~/.local/share/tuner/tuner.db` (0600), single joined writer thread, per-thread readers, in-memory `Library` snapshot for lists, 2 ms UI-thread DB budget per frame. |
| D5 | Identity | `ItemRef` = (source, kind, key) + `Fingerprint` (computed by the store); reattach §8. |
| D6 | Theme | colors.toml + shell.toml (+ user override, applied last), fc-match font + weight-correct script fallbacks, inotify reload (`try_load`, never flashes the fallback), fg-alpha tints, square corners, fixed dark OSD scrim + contrast-checked `osd_accent`. |
| D7 | Keyboard | Per-context keymaps (§15); resolution in `raw_input_hook` with event consumption; launcher idiom in lists and grids; `/`/Ctrl+K search; Esc steps out; `?` help and the status-bar hint line generated from the tables. |
| D8 | Scope | §1.1. |
| D9 | Credentials | In DB (0600); playlist/EPG caches 0700; redacted everywhere (§20). |
| D10 | Packaging | `packaging/` (§22), incl. the `render_unfocused` window rule. |

| Critique item | Resolution |
|---|---|
| glow 0.18 in protocols.md | Not a dependency. `eframe::glow` only; `Cargo.lock` has glow 0.17.0. |
| `libmpv2::render` FBO struct unsound | Not used; own wrapper (spike). |
| Wrong theme path `~/.config/omarchy/current` | `~/.local/state/omarchy/current/` (watched non-recursively). |
| egui-phosphor vs Nerd glyphs | Nerd glyphs from the UI font (`widgets::glyphs`, code points verified with `fc-query`). |
| Blit in PaintCallback vs registered texture | Registered texture (spike). Native egui viewports are forbidden (§3.1), so PaintCallback is never needed. |
| Keymap collisions | Per-context tables (§15): `f` = fullscreen in player, favourite = Ctrl+F everywhere; `←` = last channel (live) / seek (VOD); `h` = history (live) / left (guide); digits = channel entry in the live player and the guide. |
| Adaptive frame rate claim | No VRR here → judder of 25/50 fps on 60 Hz is a known v1 limitation; measured in WP-07; `mpv_options` lets users try `video-sync=display-resample` (§11.8). |
| Spike "verified" too narrow | WP-07/WP-13 acceptance adds hidden-workspace, fullscreen, 1-hour soak, audio, frame-pacing measurements (§25). |
| Event draining in `ui()` | Moved to dedicated threads (D2). **Correction (review round 2):** eframe's `logic` does NOT run while the window is on a hidden Hyprland workspace — winit-wayland reports no visibility and `RedrawRequested` needs a compositor frame callback, which Hyprland withholds from hidden workspaces. Mitigations: the `render_unfocused` window rule (frame callbacks at `misc:render_unfocused_fps`, default 15) and the controller's video watchdog (`vid=no` after 2 s without a rendered frame, resumed when the UI renders again) — §11.1. **Correction (hidden-workspace ANR):** with eframe's default vsync the UI thread did not merely idle there — it **blocked** in Mesa's `eglSwapBuffers` (swap interval 1 waits for the previous frame's callback), stopped answering `xdg_wm_base` pings, and after `misc:anr_missed_pings` Hyprland showed "application not responding" while mpv played on. Now vsync is off and frames are paced by winit frame callbacks (`pre_present_notify` in `App::ui`): hidden, the UI thread sits in its event loop and answers pings, with or without the window rule (§11.1). |
| `report_swap` omitted | Kept omitted (measured win); consequence documented in §11.8. |
| XMLTV "0.61 s" is parse-only | Import budget is end-to-end (< 8 s for 415k programmes) with batched writes + progress (§9.3); bench `benches/import.rs`, gate run by WP-13. |
| FTS schema broken (contentless + WITHOUT ROWID) | `programme` is an AUTOINCREMENT rowid table with `UNIQUE(epg_channel_id, gen, start)`; `programme_fts` is **contentless FTS5 with `contentless_delete=1`**, written in the same transaction as each programme batch and deleted by rowid before programmes are deleted; no cascades from `epg_channel` to `programme` (§6.3). Verified by `store::migrations` tests. |
| 100k M3U "< 1 s" unmeasured | Budget 1.5 s parse / 5 s end-to-end, enforced by `benches/m3u.rs` and `benches/import.rs`. |
| Text shaping & fallback (risk 1) | Script fallbacks via fontconfig (Regular weight, deduplicated by file) + Arabic reshaping and bidi reordering for labels (`fonts::display_text`); Indic shaping remains a documented limitation (§17). |
| Font weights | Separate `medium`/`bold` egui families (§17). |
| Idle inhibit undefined | Investigated Omarchy 4 idle service → Wayland idle inhibitor on our surface (§21.2). |
| Xtream `max_connections` (risk 3) | One stream at a time, stop-before-start that waits for the old stream's END_FILE, zapping delay, budget leases fed from stored account info, preview autoplay off by default, guide preview off by default (§11.5). |
| Data model only EPG | Full schema §6. |
| Architecture undesigned | §3–§5. |
| EPG grid undesigned | §13.4. |
| Responsive layout | Breakpoints scaled by Omarchy text size §12.1; wireframes per class §13. |
| Credentials in logs/URLs | §20 (`Secret<T>`, redactor, `force-media-title`, no creds on any command line, 0700 caches). |
| Image pipeline at scale | §18 (decode limits, 4 MiB/frame uploads). |
| State inventory | §13.12. |
| OSD over light themes | Fixed dark scrim `rgba(0,0,0,170)` + near-white text + `osd_accent` (theme accent only when its contrast on the scrim is ≥ 3:1). |
| Missing crates | All pinned in `Cargo.toml` (mpris-server, notify-rust, rfd, wayland-client/protocols, egui_kittest…). |
| Packaging/licence/app id | `tuner`; GPL-3.0-or-later (Arch libmpv is GPL-built) §22. |
| PiP | In-window corner mini-player (same mpv, no second connection). A separate `mpv --title=Picture-in-Picture` would cost a connection → rejected for v1. |
| Reconnect details | §11.4 table (reasons, backoff, caps). |
| Refresh while closed / server tz override | Not in v1 (flag reserved); server tz override per source in Settings ▸ Playlists ▸ Edit (§10). |
| `loadfile … replace -1 <opts>` | Verified in the local mpv 0.41 man page: index `-1` is required before options since 0.38. Values quoted with `%len%`. It returns **before** the old file stops → stop-before-start §11.5. |

---

## 3. Architecture

```
┌──────────────────────────── UI thread (eframe / egui, owns the GL context = mpv's render thread) ──────┐
│ raw_input_hook: keymap::resolve(stack) → consume resolved events (egui never sees them)                │
│ App::logic  (ONLY when the compositor sends frame callbacks — not on a hidden Hyprland workspace      │
│              unless the `render_unfocused` rule is set)                                                │
│   drain AppEvents → snapshot = player.snapshot() → VideoHost::render (mpv → FBO, stamps RenderClock)   │
│   → idle inhibit                                                                                       │
│ App::ui                                                                                                │
│   dispatch actions: overlay | screen | globals → status bar · sidebar · screen · mini-player · overlays │
│   reads: Arc<Library> (RAM), FavIndex (RAM), UI reader (≤ 2 ms/frame, NowNextCache), snapshot, images │
└──────▲───────────────────────────────────▲──────────────────────────────▲─────────────────────────────┘
       │ AppEvent + request_repaint        │ PlayerCommand (never Mpv::*) │ JobManager::submit / Writer::submit
┌──────┴──────────┐        ┌───────────────┴────────────────┐     ┌──────┴───────────────────────────────┐
│ EventBus        │◀───────│ "player" thread (controller)   │     │ tokio runtime "tuner-rt" (3 workers) │
│ (crossbeam,     │        │  PlayerMachine · zap · budget  │     │  jobs: M3U/Xtream/XMLTV import,      │
│  any thread)    │        │  stop-before-start · watchdog  │     │   details, maintenance, backup        │
└──────▲──────────┘        │  MediaResolver (own reader)    │     │  http (reqwest) · image fetches      │
       │                   │  PlayerStore → writer          │     │  MPRIS (zbus) · reminders · IPC      │
       │                   │        ▲ MpvEvent              │     │  spawn_blocking: parse/decode/match  │
       │                   │ "mpv-events" thread            │     │   and EVERY SQLite read of a job     │
       │                   │  mpv_wait_event(-1) (only waiter)    └──────┬───────────────────────────────┘
       │                   └───────────┬────────────────────┘            │ closures (one tx per batch)
       │                               ▼                                 ▼
       │                        libmpv (own demux/decode/audio threads)  "db-writer" thread (single RW conn,
       │                                                                  catch_unwind per job, joined at exit)
 "notify" watcher ── ThemeEvent (theme / LoadedFonts, built off-thread) ─┘│
                                                                          ▼
                                                     ~/.local/share/tuner/tuner.db  (WAL, 0600)
```

### 3.1 Threads

| Thread | Owns | Receives | Sends |
|---|---|---|---|
| UI (main) | GL context, `VideoSurface`, screens, `ImageCache` textures, UI DB reader, `FavIndex` | input (raw hook), `AppEvent`s | `PlayerCommand`, job submits, writer closures |
| `player` | `PlayerMachine`, `ConnectionBudget`, zap list, `MediaResolver` (own reader), `PlayerStore` | `PlayerCommand`, `MpvEvent`, 250 ms tick | mpv commands, `watch<Arc<PlayerSnapshot>>`, `AppEvent::Player`, progress/history/failure writes |
| `mpv-events` | nothing (borrows `Arc<Mpv>`) | `mpv_wait_event` | `MpvEvent` → player |
| `db-writer` | the only read-write SQLite connection | `FnOnce(&mut Connection)` closures, shutdown sentinel | results via oneshot; `AppEvent::Data` on `submit_notify` |
| `tuner-rt` ×3 + blocking pool | HTTP client, jobs, MPRIS server, reminder timer, IPC server, image fetch/decode | job/requests | `AppEvent`s, writer closures |
| `fonts`, `theme` (startup only) | — | — | `LoadedFonts`, `Theme`, joined in the creation callback |
| notify (inside `notify` crate) | inotify watches | fs events | `ThemeEvent` (reload done off-thread) |

Rules:
* **The UI thread is mpv's render thread.** It never calls `Mpv::*` (only `Player::render_core`
  once, for `mpv_render_context_create`), never blocks on the controller, and never does I/O
  except indexed SQLite point/window queries inside the 2 ms per-frame `FrameBudget`.
* Background components never wait on the UI and never assume an event is processed promptly
  (the UI may not run at all while hidden).
* Every cross-thread message type is in `msg.rs`, `player/mod.rs` or `jobs/mod.rs`.
* **One native viewport only.** eframe makes the GL context current before `logic` only with a
  single viewport, and `VideoHost::render` relies on it; egui `show_viewport_*` is forbidden.
* Nothing in `logic`/`ui` may be required for correctness while hidden: progress saving,
  reconnects, MPRIS and reminders live on other threads.

### 3.2 Data flows

* **Refresh a source**: UI `jobs.submit(RefreshSource)` → job downloads to a 0700 cache file
  (progress events) → parse in `spawn_blocking` → batches of 2 000 rows → writer (one tx each) →
  `finish_catalogue` (sweep, reattach, counts, resolve EPG) → provider EPG refresh if stale
  (follow-up job via `JobContext::submit`) → `RebuildLibrary` → `JobOutcome::Library(Arc<Library>)`
  → UI swaps the snapshot and its `FavIndex`, clears the search index;
  `JobOutcome::SourceRefreshed { account }` → `PlayerCommand::SetConnectionLimit`.
* **Play**: UI `PlayerCommand::Play{media, zap, origin, immediate}` → controller resolves
  `MediaRef → Playable` with its own reader → `PlayerMachine` (Zapping → Loading) →
  (stop-before-start if same limited source) → `loadfile url replace -1 opts` → entry id read →
  events → Playing → snapshot published (+ repaint); UI paints the FBO texture wherever the
  current layout puts the video.
* **Media key while hidden**: Omarchy `omarchy-shell media next` → MPRIS (tokio) →
  `PlayerCommand::Zap` → controller resolves/loads; UI not involved. Without the
  `render_unfocused` rule no frames are rendered; after 2 s the controller sets `vid=no`
  (audio continues) and the app sends `ResumeVideo` as soon as `logic` runs again.
* **Theme switch**: `omarchy-theme-set` replaces `current/theme`, writes `theme.name` last →
  inotify → debounce 180 ms → `Theme::try_load` on a worker (error → keep current, retry in
  300 ms) → `ThemeEvent::Changed(Arc<Theme>)` → UI applies egui style in the same frame.
* **Settings edit**: screen `ctx.edit_config(f)` → app applies, saves `config.toml`, swaps the
  shared `Arc<Config>`, sends `PlayerCommand::ApplyConfig` (all v1 settings apply live).

---

## 4. Module map

Every row is one work package's territory (§24.2 is the authoritative ownership table).
"Frozen" = architect-owned (WP-00); changing it needs sign-off and re-tags `skeleton-v1.N`.

| Path | Owner | Responsibility / contract |
|---|---|---|
| `lib.rs`, `model/**`, `msg.rs`, `error.rs`, `config.rs`, `paths.rs`, `cli.rs` | WP-00 (frozen) | Core data types, cross-thread messages, error type (Display is always redacted), config schema (all v1 settings), XDG paths (0700 credential caches), CLI. |
| `store/mod.rs`, `store/migrations*`, `store/library.rs` index types, `store/sources.rs::{register_secrets, account_limits}` | WP-00 (frozen, implemented) | `Store::open`/`open_temp`/`close`, writer actor (catch_unwind, sentinel shutdown + join), readers, schema; in-memory `LiveIndex`/`VodIndex`/`FavIndex` accessors + `from_parts`. |
| `store/{sources,live,vod,epg,user,import,library(load),backup,sql}.rs` | WP-02 | Repo functions over `&Connection`; import sinks; `library::load`; backup format. |
| `sources/identity.rs`, `sources/mod.rs::sniff_payload`, `sources/http.rs::sniff_compression`, `sources/catchup.rs::parse_kind`, `sources/xtream/urls.rs` | WP-00 (frozen, implemented) | Stable keys/normalization/fingerprints (golden), payload & compression sniffing, catch-up kind parsing, Xtream URL builders + server normalization. |
| `sources/{m3u,smart_names}.rs` | WP-03 | Streaming M3U parser; name cleanup. (WP-03 also owns the identity golden/property tests.) |
| `sources/http.rs` (rest), `sources/xtream/{client,types}.rs` | WP-04 | reqwest client, streamed downloads (0600 `.part`), lenient Xtream types, client. |
| `sources/{xmltv,epg_match,catchup}.rs` (rest) | WP-05a | XMLTV streaming parser; channel matching + lazy suggestions; catch-up URL templates. |
| `sources/resolve.rs` | WP-05b | `MediaRef → Playable`. |
| `jobs/**` | WP-06 | Job manager (coalesce/rerun, cancel, progress, follow-up submit), pipelines, details, maintenance, probe/add, scheduler. |
| `player/**` | WP-07 | mpv init, render (implemented), event pump (implemented), controller + state machine (implemented, WP-07 hardens), reconnect, budget, tracks/stream info. |
| `platform/**`, `ipc.rs`, `redact.rs`, `logging.rs`, `packaging/**` | WP-08 | MPRIS, idle inhibit, notifications/reminders, file picker, Hyprland focus, single-instance IPC, redaction, logging, desktop files. |
| `images.rs`, `search.rs` (rest; `ListFilter` implemented) | WP-09 | Image cache pipeline; nucleo global search, FTS EPG search. |
| `keymap.rs`, `ui/layout.rs`, `ui/widgets/**` (− `poster.rs`), `ui/overlays/{help,jobs,dialogs}.rs`, `examples/keymap_md.rs` | WP-10a | Key tables & resolution, component catalogue, generic overlays. |
| `app.rs`, `services.rs`, `main.rs` | WP-10b | Integration: dispatch, navigation, data-scope reactions, CLI/IPC, connection limits, reminders refresh. |
| `ui/mod.rs`, `ui/data.rs`, `ui/testing.rs`, `ui/screens/mod.rs`, `ui/overlays/mod.rs` (incl. `build`), `ui/parental.rs` session logic | WP-00 (frozen, implemented) | UI contracts: routes, `UiCtx`, `FrameBudget`, `EpgReader`, `NowNextCache`, `TestUi`, overlay factory, PIN session. |
| `ui/{epg_grid,video_host}.rs`, `ui/screens/{live,guide,player}.rs`, `ui/overlays/{programme,tracks,edit_item}.rs` | WP-11 | Live TV, guide grid, fullscreen player/OSD, mini-player, Edit information. |
| `ui/screens/{home,movies,series,movie_detail,series_detail,settings,onboarding,epg_mapping,logs}.rs`, `ui/widgets/poster.rs`, `ui/overlays/{search,resume}.rs` | WP-12 | Home, VOD, settings, onboarding, search UI, PIN UI, logs, EPG mapping. |
| `theme/**`, `fonts.rs` | WP-01 | Theme parsing/tokens/egui mapping (implemented core), live reload, fonts + fallbacks + RTL helper. |

Cross-module contracts worth repeating:
* `UiCtx` (ui/mod.rs) is the only thing screens get; config changes go through
  `UiCtx::edit_config`; navigation through `UiCtx::navigate/push/open`; player control through
  `UiCtx::play`; EPG reads through `UiCtx::epg_reader` + `now_next` within `budget`.
* Screens implement `ui::Screen`; overlays implement `ui::overlays::Overlay`; both declare their
  key contexts and receive resolved `keymap::Action`s — **no screen reads raw keys**.
* All DB writes go through `store::Writer` (never a second RW connection); upserts are
  `INSERT … ON CONFLICT … DO UPDATE`, never `INSERT OR REPLACE` on FK-referenced tables.
* Anything that may contain a credential is a `Secret<String>` or is passed through
  `redact()` before it can be displayed or logged.

---

## 5. Runtime details

### 5.1 Startup & shutdown

```
main: Cli::parse → AppPaths (or --profile-dir) → Config::load (bad file → defaults + error log,
      file untouched) → logging::init → [IPC handoff: if another instance answers → exit 0]
      → EventBus → spawn threads: FontSet::resolve+load (~145 ms), Theme::load (hyprctl)
      → Services::start (in parallel with those threads):
          paths.ensure (0700 data/state/run/playlists/epg) → tokio rt → Store::open (0600, WAL,
          migrate) → register_secrets → HttpClient → JobManager → Player::start (mpv init,
          event pump, controller)
      → eframe::run_native(glow, app_id "tuner")
creation callback (GL current): join fonts/theme threads → LoadedFonts::install →
      egui_style::apply → VideoSurface::new(player.render_core()) → UI reader → ImageCache →
      start_integrations (theme watcher, IPC server, MPRIS, reminders, scheduler, connection
      limits from stored AccountInfo) → idle inhibitor → start route (no sources ⇒
      Onboarding; StartScreen::Auto ⇒ Live until favourites/history exist, then Home) →
      RebuildLibrary job (async; lists show skeletons until it lands) → CLI/IPC request
```
First frame target: < 300 ms after launch regardless of library size (library loads async;
measured by WP-13 with a 100k-channel DB).

Shutdown (`on_exit`): `VideoSurface::destroy` (GL current: render ctx, then FBO) →
`Services::shutdown`: integrations stop → `jobs.cancel_all` → `Player::shutdown` (Stop → final
progress save submitted, join controller, stop pump, drop mpv) → runtime `shutdown_timeout(2 s)`
→ `Store::close(3 s)`: the writer runs everything queued before the sentinel (incl. that final
save), checkpoints the WAL (TRUNCATE) and is joined. (WP-13: `jobs.shutdown` refuses new jobs
and cancels; after the player, the app waits ≤ 5 s for cancelled jobs to wind down — an aborted
guide import discards its partial generation and restores channel matches — before the runtime
goes. SIGTERM/SIGINT/SIGHUP close the window the same way; if no frame runs within 3 s (hidden
workspace) or a second signal arrives — also during a stuck shutdown — the signal watcher stops
the player, flushes the writer and the log file, removes `run/tuner.sock` and exits 0.)
Unwind safety: `TunerApp` declares `video` before `services`; `MpvGl` frees its context only in
an explicit `destroy` (GL current) and otherwise leaks it; `Player` leaks mpv while a render
context is alive — never UB on a panic path.

### 5.2 Jobs

`JobKind` enumerates every background operation (refresh source/EPG, refresh stale, probe/add
source, lazy VOD/series details, rematch EPG, rebuild library, maintenance, backup export/
import). `submit` returns a `JobId` immediately. Coalescing: identical jobs (`dedupe_key`)
coalesce only while the existing one is `Queued`/`Connecting` **and has not read its inputs
yet**; later, a `rerun` flag resubmits it after it finishes (a newer request is never dropped).
*Decided in phase 2:* a refresh stays `Connecting` while it talks to the server, so it marks the
moment it reads its source row (`Progress::mark_inputs_read`); a request arriving after that
runs again in full after the current run instead of being merged into it (it may carry edits
the running job never saw). Follow-ups go through
`JobContext::submit` (refresh → provider EPG → RebuildLibrary; restore → refresh each source;
AddSource → RefreshSource). `JobContext::player_busy` (set by the app from each snapshot) makes
the scheduler skip ticks while a stream is Loading/Zapping. Semaphores: playlists 2, EPG 1,
details 4, other 2. Heavy CPU **and every SQLite read** run in `spawn_blocking` with their own
reader (never on the 3 async workers). Progress (`JobPhase`: Queued, Connecting,
Downloading{done,total}, Parsing{items}, Writing{done,total}, Matching, Indexing, Finishing) is
throttled to 10 Hz per job. Cancellation via `CancellationToken` checked every 64 KiB / 1 000
items. `JobEvent::Finished` carries a redacted error string or a `JobOutcome`
(`SourceRefreshed` includes fresh `AccountInfo`). UI: status-bar summary, Ctrl+J panel, per-source
rows in Settings. `Progress::detached()` exists for unit tests.

### 5.3 Database connections

* One writer thread; closures run FIFO inside `catch_unwind` (a panicking job is rolled back,
  its ticket errors, the writer keeps going); long imports are split into ~2 000-row (catalogue)
  or ~5 000-row (EPG, programmes + FTS rows together) transactions so a favourite toggle waits
  at most one batch (~20 ms). **No transaction may be larger than one batch** — the EPG
  generation flip is a few UPDATEs, sweeps are chunked.
* `Store::reader()` per thread (`query_only`, `NO_MUTEX`); WAL means readers see the last
  committed state and never block the writer. Readers must not keep statements open across
  frames (WAL growth).
* PRAGMAs: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, `temp_store=MEMORY`,
  `cache_size=-32000` (writer) / `-16000` (readers), `wal_autocheckpoint=4000`,
  `journal_size_limit=64 MiB`, busy timeout 5 s. Maintenance runs `wal_checkpoint(TRUNCATE)`.
* **UI-thread budget: 2 ms of SQLite per frame** (`ui::data::FrameBudget`, measured with
  `Instant`; the UI thread is mpv's render thread and runs at the video frame rate while video is
  visible). Now/next goes through `NowNextCache` (key = (epg channel, tvg-shift), valid until
  the current programme ends, ≤ 8 misses per frame); guide buckets through `ProgrammeCache`
  (≤ 24 loads per frame, same budget). Deferred work paints skeletons and requests a repaint.
* Tests use `Store::open_temp` (real WAL file in a private temp dir) — shared-cache in-memory
  databases return SQLITE_LOCKED, which `busy_timeout` ignores, and have no WAL.

### 5.4 In-memory library

`store::library::Library` = `sources: Vec<SourceMeta>` (no secrets) + `LiveIndex` (all channel
summaries, groups, ordered members, by-id/by-ref maps, hidden set) + `VodIndex` + `favs`
(build-time copy) + `recent_channels`. Built off-thread (`RebuildLibrary`), swapped as
`Arc<Library>`. Budget: 100k channels + 50k movies + 20k series → < 400 ms, < 60 MB. The index
types and accessors are implemented in the skeleton and built with `from_parts`.

Favourites live OUTSIDE the immutable snapshot: the app owns `favs: FavIndex` (copied from each
new `Library`), exposed as `UiCtx::favs`; a toggle submits `store::user::set_favourite` and
patches `favs.apply_toggle` immediately; `DataScope::Favourites` reloads it with
`library::load_favs` (event-driven, ≤ 5 ms). All list ordering (`LiveIndex::collection(c, favs,
recent, sort)`) happens here, so zapping order == list order.

| `DataScope` | App reaction |
|---|---|
| Live, Vod, Overrides, Sources, All | `RebuildLibrary` |
| History (every zap) | re-read `recent_channels` (≤ 500 history rows) → `Library::with_recent_channels` (shares the `Arc` indexes, keeps `built_at`, new `generation` for screens showing the recent list); no rebuild (WP-13) |
| Epg | `NowNextCache::invalidate` + `RebuildLibrary` (`ChannelSummary.epg_channel_id` is a snapshot of `channel_epg`) |
| Favourites | `load_favs` → `TunerApp.favs` |
| Reminders | `ReminderScheduler::refresh` |
| VodDetails, Progress | screens react in `on_event` |

---

## 6. Data model

### 6.1 Entities

```
source 1─* grp 1─* channel_group *─1 channel ─0..1 channel_epg ─1 epg_channel *─1 epg_source
source 1─* movie (movie_group) · source 1─* series 1─* season · series 1─* episode
epg_channel 1─* epg_display_name · epg_channel 1─* programme ── programme_fts (contentless, by rowid)
source 1─* channel_epg_map (by channel KEY + fp) *─1 epg_source
user data (by ItemRef = source_id, kind, item_key):
  fav_list 1─* fav_item · item_override · group_override (by group key) · watch_progress
  · history · reminder (by channel key) · stream_failure
  (every item-keyed user table carries fp + orphaned_since)
meta (k/v: backup id, last library build, etc.)
```

### 6.2 Schema (verbatim from `src/store/migrations/0001_init.sql`)

```sql
-- Tuner schema v1. FROZEN at tag `skeleton-v1`; later changes are appended as 0002_*.sql
-- (owner WP-02, architect sign-off) — never edit this file after the tag.
-- Conventions: timestamps = unix seconds UTC; JSON columns hold serde_json of model types;
-- `seen_gen` / `gen` implement mark-and-sweep imports (DESIGN §7.2, §9.3).
-- User data is keyed by (source_id, kind, item_key) — NEVER by catalogue row ids — so it
-- survives refreshes; `fp` holds the JSON Fingerprint used for reattach (DESIGN §8). Every
-- user-data table that is keyed by an item carries `fp` + `orphaned_since`.
-- WRITE RULES (DESIGN §6.3/§7.2): all upserts are `INSERT … ON CONFLICT(…) DO UPDATE`;
-- `INSERT OR REPLACE` is FORBIDDEN on any table referenced by a foreign key (it deletes the
-- parent row, fires cascades and changes row ids).

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ---------------------------------------------------------------- sources
CREATE TABLE source (
  id            INTEGER PRIMARY KEY,
  name          TEXT    NOT NULL,
  kind          TEXT    NOT NULL CHECK (kind IN ('m3u-url','m3u-file','xtream')),
  icon          TEXT    NOT NULL DEFAULT '󰐹',
  color         TEXT    NOT NULL DEFAULT 'accent',
  url           TEXT,                 -- M3U URL (secret: may embed credentials)
  path          TEXT,                 -- M3U file
  xt_server     TEXT,
  xt_username   TEXT,
  xt_password   TEXT,                 -- plain; DB file is 0600 (DESIGN §20)
  settings      TEXT    NOT NULL DEFAULT '{}',   -- JSON SourceSettings
  enabled       INTEGER NOT NULL DEFAULT 1,
  sort_index    INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  last_refresh  INTEGER,
  last_attempt  INTEGER,
  last_error    TEXT,                 -- redacted
  etag          TEXT,
  last_modified TEXT,
  account       TEXT,                 -- JSON AccountInfo
  generation    INTEGER NOT NULL DEFAULT 0,      -- last *completed* catalogue import
  n_channels    INTEGER NOT NULL DEFAULT 0,
  n_movies      INTEGER NOT NULL DEFAULT 0,
  n_series      INTEGER NOT NULL DEFAULT 0,
  n_groups      INTEGER NOT NULL DEFAULT 0
);

-- ---------------------------------------------------------------- catalogue
CREATE TABLE grp (
  id             INTEGER PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind           TEXT    NOT NULL CHECK (kind IN ('channel','movie','series')),
  key            TEXT    NOT NULL,     -- GroupKey: normalized name
  name           TEXT    NOT NULL,
  provider_id    TEXT,                 -- Xtream category_id
  provider_order INTEGER NOT NULL,
  item_count     INTEGER NOT NULL DEFAULT 0,
  seen_gen       INTEGER NOT NULL,
  UNIQUE (source_id, kind, key)
);

CREATE TABLE channel (
  id             INTEGER PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  key            TEXT    NOT NULL,     -- ItemKey
  provider_id    TEXT,
  name           TEXT    NOT NULL,
  clean_name     TEXT    NOT NULL,
  norm_name      TEXT    NOT NULL,     -- identity::normalize_name
  number         INTEGER,
  logo           TEXT,
  tvg_id         TEXT,
  tvg_name       TEXT,
  tvg_shift_min  INTEGER NOT NULL DEFAULT 0,
  stream         TEXT    NOT NULL,     -- JSON StreamLocator
  url_key        TEXT,                 -- identity::url_key (reattach)
  headers        TEXT,                 -- JSON HttpHeaders
  catchup        TEXT,                 -- JSON CatchupSpec
  radio          INTEGER NOT NULL DEFAULT 0,
  unsupported    TEXT,
  provider_order INTEGER NOT NULL,
  added_at       INTEGER,
  seen_gen       INTEGER NOT NULL,
  UNIQUE (source_id, key)
);
CREATE INDEX channel_tvg      ON channel(tvg_id);
CREATE INDEX channel_norm     ON channel(source_id, norm_name);
CREATE INDEX channel_provider ON channel(source_id, provider_id);
CREATE INDEX channel_sweep    ON channel(source_id, seen_gen);
CREATE INDEX channel_urlkey   ON channel(source_id, url_key);   -- reattach step 4

CREATE TABLE channel_group (
  group_id   INTEGER NOT NULL REFERENCES grp(id)     ON DELETE CASCADE,
  channel_id INTEGER NOT NULL REFERENCES channel(id) ON DELETE CASCADE,
  position   INTEGER NOT NULL,
  is_primary INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (group_id, channel_id)
) WITHOUT ROWID;
CREATE INDEX channel_group_ch ON channel_group(channel_id);

CREATE TABLE movie (
  id             INTEGER PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  key            TEXT    NOT NULL,
  provider_id    TEXT,
  name           TEXT    NOT NULL,
  clean_name     TEXT    NOT NULL,
  norm_name      TEXT    NOT NULL,
  year           INTEGER,
  poster         TEXT,
  backdrop       TEXT,
  rating         REAL,
  plot           TEXT,
  genre          TEXT,
  cast_          TEXT,
  director       TEXT,
  duration_s     INTEGER,
  release_date   TEXT,
  tmdb_id        TEXT,
  trailer        TEXT,
  stream         TEXT    NOT NULL,
  url_key        TEXT,
  provider_order INTEGER NOT NULL,
  added_at       INTEGER,
  details_at     INTEGER,
  seen_gen       INTEGER NOT NULL,
  UNIQUE (source_id, key)
);
CREATE INDEX movie_added ON movie(source_id, added_at DESC);
CREATE INDEX movie_norm  ON movie(source_id, norm_name);
CREATE INDEX movie_sweep ON movie(source_id, seen_gen);
CREATE INDEX movie_urlkey   ON movie(source_id, url_key);
CREATE INDEX movie_provider ON movie(source_id, provider_id);

CREATE TABLE movie_group (
  group_id INTEGER NOT NULL REFERENCES grp(id)   ON DELETE CASCADE,
  movie_id INTEGER NOT NULL REFERENCES movie(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  PRIMARY KEY (group_id, movie_id)
) WITHOUT ROWID;
CREATE INDEX movie_group_m ON movie_group(movie_id);

CREATE TABLE series (
  id             INTEGER PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  key            TEXT    NOT NULL,
  provider_id    TEXT,
  name           TEXT    NOT NULL,
  clean_name     TEXT    NOT NULL,
  norm_name      TEXT    NOT NULL,
  year           INTEGER,
  cover          TEXT,
  backdrop       TEXT,
  rating         REAL,
  plot           TEXT,
  genre          TEXT,
  cast_          TEXT,
  director       TEXT,
  release_date   TEXT,
  last_modified  INTEGER,
  provider_order INTEGER NOT NULL,
  added_at       INTEGER,
  details_at     INTEGER,
  seen_gen       INTEGER NOT NULL,
  UNIQUE (source_id, key)
);
CREATE INDEX series_added ON series(source_id, added_at DESC);
CREATE INDEX series_norm  ON series(source_id, norm_name);
CREATE INDEX series_sweep ON series(source_id, seen_gen);
CREATE INDEX series_provider ON series(source_id, provider_id);

CREATE TABLE series_group (
  group_id  INTEGER NOT NULL REFERENCES grp(id)    ON DELETE CASCADE,
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  position  INTEGER NOT NULL,
  PRIMARY KEY (group_id, series_id)
) WITHOUT ROWID;
CREATE INDEX series_group_s ON series_group(series_id);

CREATE TABLE season (
  series_id     INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  number        INTEGER NOT NULL,
  name          TEXT,
  cover         TEXT,
  air_date      TEXT,
  overview      TEXT,
  episode_count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (series_id, number)
) WITHOUT ROWID;

CREATE TABLE episode (
  id          INTEGER PRIMARY KEY,
  series_id   INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  key         TEXT    NOT NULL,
  provider_id TEXT,
  season      INTEGER NOT NULL,
  episode     INTEGER NOT NULL,
  title       TEXT    NOT NULL,
  plot        TEXT,
  duration_s  INTEGER,
  still       TEXT,
  air_date    TEXT,
  rating      REAL,
  stream      TEXT    NOT NULL,
  UNIQUE (source_id, key)
);
CREATE INDEX episode_series ON episode(series_id, season, episode);

-- ---------------------------------------------------------------- EPG
CREATE TABLE epg_source (
  id                 INTEGER PRIMARY KEY,
  name               TEXT    NOT NULL,
  kind               TEXT    NOT NULL CHECK (kind IN ('url','file','provider')),
  url                TEXT,                -- secret (xmltv.php carries credentials)
  path               TEXT,
  playlist_source_id INTEGER REFERENCES source(id) ON DELETE CASCADE,  -- kind='provider'
  priority           INTEGER NOT NULL DEFAULT 100,
  offset_min         INTEGER NOT NULL DEFAULT 0,
  enabled            INTEGER NOT NULL DEFAULT 1,
  refresh_h          INTEGER,
  active_gen         INTEGER NOT NULL DEFAULT 0,
  last_refresh       INTEGER,
  last_attempt       INTEGER,
  last_error         TEXT,
  etag               TEXT,
  last_modified      TEXT,
  n_channels         INTEGER NOT NULL DEFAULT 0,
  n_programmes       INTEGER NOT NULL DEFAULT 0,
  max_stop           INTEGER
);

CREATE TABLE epg_channel (
  id            INTEGER PRIMARY KEY,
  epg_source_id INTEGER NOT NULL REFERENCES epg_source(id) ON DELETE CASCADE,
  xmltv_id      TEXT    NOT NULL,
  icon          TEXT,
  seen_gen      INTEGER NOT NULL,
  UNIQUE (epg_source_id, xmltv_id)
);
CREATE INDEX epg_channel_nocase ON epg_channel(xmltv_id COLLATE NOCASE);

CREATE TABLE epg_display_name (
  epg_channel_id INTEGER NOT NULL REFERENCES epg_channel(id) ON DELETE CASCADE,
  name           TEXT    NOT NULL,
  norm           TEXT    NOT NULL,     -- identity::normalize_name
  PRIMARY KEY (epg_channel_id, name)
) WITHOUT ROWID;
CREATE INDEX epg_display_norm ON epg_display_name(norm);

-- Rowid table ON PURPOSE: programme.id is the FTS5 rowid. AUTOINCREMENT so a deleted id is
-- never reused (a stale FTS row can never match a new programme). The FK to epg_channel has NO
-- cascade: programme deletes are always explicit and chunked, FTS rows first
-- (store::epg::delete_programmes_of); deleting an epg_channel / epg_source / source that still
-- has programmes fails loudly instead of silently desynchronising the FTS index.
CREATE TABLE programme (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  epg_source_id  INTEGER NOT NULL,     -- denormalized for per-generation sweeps
  epg_channel_id INTEGER NOT NULL REFERENCES epg_channel(id),
  gen            INTEGER NOT NULL,
  start          INTEGER NOT NULL,
  stop           INTEGER NOT NULL,
  title          TEXT    NOT NULL,
  sub_title      TEXT,
  descr          TEXT,
  categories     TEXT,                 -- '\x1f'-joined
  icon           TEXT,
  season         INTEGER,
  episode        INTEGER,
  rating         TEXT,
  star_rating    TEXT,
  catchup_id     TEXT,
  flags          INTEGER NOT NULL DEFAULT 0   -- bit0 new, bit1 live, bit2 previously-shown
);
-- UNIQUE: duplicate (channel, start) inside one generation → first wins
-- (`INSERT … ON CONFLICT(epg_channel_id, gen, start) DO NOTHING`).
CREATE UNIQUE INDEX programme_grid ON programme(epg_channel_id, gen, start);
CREATE INDEX programme_sweep ON programme(epg_source_id, gen);
CREATE INDEX programme_stop  ON programme(stop);

-- Contentless FTS5 (SQLite ≥ 3.43 `contentless_delete`) over title/sub_title, keyed by
-- programme.id. Rows are inserted in the SAME transaction as their programme batch and deleted
-- by rowid (`DELETE FROM programme_fts WHERE rowid IN (…)`) before the programmes are deleted.
-- Hits are joined back to `programme` (and filtered to the active generation) for display.
-- No triggers (they would slow the 400k-row bulk import several-fold); no 'rebuild' needed.
CREATE VIRTUAL TABLE programme_fts USING fts5(
  title, sub_title,
  content = '', contentless_delete = 1,
  tokenize = 'unicode61 remove_diacritics 2',
  prefix = '2 3'
);

-- Channel → EPG mappings, keyed by STABLE channel identity (survives refresh).
CREATE TABLE channel_epg_map (
  source_id     INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  channel_key   TEXT    NOT NULL,
  epg_source_id INTEGER NOT NULL REFERENCES epg_source(id) ON DELETE CASCADE,
  xmltv_id      TEXT    NOT NULL,
  method        TEXT    NOT NULL,      -- EpgMatchMethod (kebab)
  confidence    REAL    NOT NULL DEFAULT 1.0,
  fp            TEXT    NOT NULL DEFAULT '{}',   -- channel Fingerprint (manual rows reattach)
  orphaned_since INTEGER,
  PRIMARY KEY (source_id, channel_key, epg_source_id)
) WITHOUT ROWID;

-- Resolved mapping per channel row (derived; rebuilt by store::epg::resolve_mappings).
CREATE TABLE channel_epg (
  channel_id     INTEGER PRIMARY KEY REFERENCES channel(id) ON DELETE CASCADE,
  epg_channel_id INTEGER NOT NULL REFERENCES epg_channel(id) ON DELETE CASCADE
);
CREATE INDEX channel_epg_rev ON channel_epg(epg_channel_id);

-- ---------------------------------------------------------------- user data
CREATE TABLE fav_list (
  id         INTEGER PRIMARY KEY,
  name       TEXT    NOT NULL,
  icon       TEXT    NOT NULL DEFAULT '',
  sort_index INTEGER NOT NULL DEFAULT 0,
  is_default INTEGER NOT NULL DEFAULT 0
);
INSERT INTO fav_list (name, icon, sort_index, is_default) VALUES ('Favourites', '', 0, 1);

CREATE TABLE fav_item (
  list_id        INTEGER NOT NULL REFERENCES fav_list(id) ON DELETE CASCADE,
  source_id      INTEGER NOT NULL REFERENCES source(id)   ON DELETE CASCADE,
  kind           TEXT    NOT NULL,
  item_key       TEXT    NOT NULL,
  position       INTEGER NOT NULL,
  added_at       INTEGER NOT NULL,
  fp             TEXT    NOT NULL,     -- JSON Fingerprint
  orphaned_since INTEGER,
  PRIMARY KEY (list_id, source_id, kind, item_key)
) WITHOUT ROWID;
CREATE INDEX fav_item_ref ON fav_item(source_id, kind, item_key);

CREATE TABLE item_override (
  source_id        INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind             TEXT    NOT NULL,
  item_key         TEXT    NOT NULL,
  rename           TEXT,
  hidden           INTEGER NOT NULL DEFAULT 0,
  locked           INTEGER NOT NULL DEFAULT 0,
  user_order       INTEGER,
  catchup_offset_s INTEGER,
  fp               TEXT    NOT NULL,
  orphaned_since   INTEGER,
  PRIMARY KEY (source_id, kind, item_key)
) WITHOUT ROWID;

CREATE TABLE group_override (
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind        TEXT    NOT NULL,
  group_key   TEXT    NOT NULL,
  rename      TEXT,
  hidden      INTEGER NOT NULL DEFAULT 0,
  locked      INTEGER NOT NULL DEFAULT 0,
  user_order  INTEGER,
  pinned_home INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (source_id, kind, group_key)
) WITHOUT ROWID;

CREATE TABLE watch_progress (
  source_id  INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind       TEXT    NOT NULL,
  item_key   TEXT    NOT NULL,
  position_s REAL    NOT NULL,
  duration_s REAL,
  finished   INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL,
  title      TEXT    NOT NULL,
  fp         TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  PRIMARY KEY (source_id, kind, item_key)
) WITHOUT ROWID;
CREATE INDEX watch_progress_recent ON watch_progress(updated_at DESC);

CREATE TABLE history (
  id        INTEGER PRIMARY KEY,
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind      TEXT    NOT NULL,
  item_key  TEXT    NOT NULL,
  title     TEXT    NOT NULL,
  at        INTEGER NOT NULL
);
CREATE INDEX history_at ON history(at DESC);

CREATE TABLE reminder (
  id           INTEGER PRIMARY KEY,
  source_id    INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  channel_key  TEXT    NOT NULL,
  channel_name TEXT    NOT NULL,
  title        TEXT    NOT NULL,
  start        INTEGER NOT NULL,
  stop         INTEGER NOT NULL,
  lead_s       INTEGER NOT NULL DEFAULT 120,
  state        TEXT    NOT NULL DEFAULT 'pending',
  fp           TEXT    NOT NULL DEFAULT '{}',   -- channel Fingerprint
  orphaned_since INTEGER,
  UNIQUE (source_id, channel_key, start)
);
CREATE INDEX reminder_due ON reminder(state, start);

-- Per-channel stream failure cache (critique gap 11): dims dead channels, skips them when zapping.
CREATE TABLE stream_failure (
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  item_key  TEXT    NOT NULL,
  reason    TEXT    NOT NULL,          -- PlayerErrorKind (kebab)
  at        INTEGER NOT NULL,
  count     INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (source_id, item_key)
) WITHOUT ROWID;
```

### 6.3 FTS approach and write rules (fixes the critique and review B2)

* `programme` is an `INTEGER PRIMARY KEY AUTOINCREMENT` rowid table: ids are never reused, so
  a stale FTS row can never match a newer programme. `UNIQUE(epg_channel_id, gen, start)`
  (`programme_grid`, also the window-query index) makes duplicates impossible; the importer
  uses `INSERT … ON CONFLICT(epg_channel_id, gen, start) DO NOTHING` (**first wins**).
* `programme_fts` is **contentless** FTS5 (`content=''`, `contentless_delete=1`, SQLite ≥
  3.43; bundled 3.53) over `title, sub_title` (`unicode61 remove_diacritics 2`, prefix
  indexes 2/3). Descriptions are not indexed (size; v1 search targets titles).
* **Insert**: every `write_programmes` batch inserts its programmes and, in the same
  transaction, `INSERT INTO programme_fts(rowid, title, sub_title)` for the rows actually
  inserted. No big "fill FTS for the generation" transaction exists anywhere.
* **Delete** (generation sweep, prune, abort, EPG-source / playlist-source removal): in chunks,
  `DELETE FROM programme_fts WHERE rowid IN (SELECT id … LIMIT n)` then `DELETE FROM programme
  WHERE id IN (…)`, same transaction. `programme.epg_channel_id` has **no** `ON DELETE
  CASCADE`: deleting an `epg_channel`, `epg_source` or `source` that still has programmes
  fails, so `store::epg::delete_programmes_of(scope)` must run first — the FK enforces the
  discipline instead of silently desynchronising the index.
* **Maintenance**: `DELETE FROM programme_fts WHERE rowid NOT IN (SELECT id FROM programme)`
  (`epg::fts_cleanup`, expected 0). No `'rebuild'` (impossible for contentless tables and
  seconds-long on the writer anyway).
* **Query**: user text → tokens → each quoted, last one with `*` (`"tom" "jer"*`); join
  `programme p ON p.id = f.rowid` → `epg_channel` → `epg_source` with `p.gen = es.active_gen`;
  order airing now, upcoming, past; join `channel_epg` for playable channels. Tested in
  `store::migrations::tests` (join back, delete by rowid, no reuse, FK discipline).
* **Write rules for every table**: upserts are `INSERT … ON CONFLICT(…) DO UPDATE`.
  `INSERT OR REPLACE` is forbidden on any table referenced by a foreign key (`source`, `grp`,
  `channel`, `movie`, `series`, `epg_source`, `epg_channel`, `fav_list`): REPLACE deletes
  the parent row, fires cascades (e.g. wipes the live guide mid-import) and changes row ids the
  Library depends on.

### 6.4 Size & performance targets

| Data | Rows | DB size (approx.) | Target |
|---|---|---|---|
| Channels | 100k | 60 MB | upsert 100k < 3 s; library load < 400 ms |
| Movies / series | 50k / 20k | 40 MB | upsert < 2 s |
| Programmes | 415k (US epg.pw) | 150 MB + FTS 25 MB | parse 0.6 s + write < 6 s; window query < 1 ms |

---

## 7. Storage lifecycle

### 7.1 Migrations
`PRAGMA user_version`; `store::migrations::MIGRATIONS` is append-only; each script runs in one
transaction; a DB newer than the binary is refused (never downgraded silently). `0001_init.sql`
is frozen at tag `skeleton-v1` (nothing has shipped before it, which is why review round 2
edited it in place); later changes are new `000N_*.sql` entries appended by WP-02 with
architect sign-off.

### 7.2 Mark-and-sweep imports (catalogue)
`source.generation` = last completed import. An import uses `generation + 1`, **upserts** rows
by `(source_id, key)` setting `seen_gen` (so unchanged items keep their row ids and every
in-memory reference stays valid), and only on success of a *kind* deletes rows of that kind with
`seen_gen < generation`. Failed or cancelled imports never sweep — a provider outage never
empties your lists.

### 7.3 Generations (EPG)
`epg_source.active_gen`; readers filter `programme.gen = active_gen` (joined through
`epg_channel → epg_source`), the import writes `active_gen + 1` (programmes + FTS rows per
batch), `finish_epg_import` flips it in one SHORT transaction (atomic for readers), the old
generation is swept in 20 000-row chunks afterwards (FTS rows first), and only then are epg
channels not seen in the active generation deleted (their `channel_epg` rows cascade; mapping
resolution already ignores them). An aborted import deletes its own generation, chunked.

### 7.4 Pruning
Daily `Maintenance`: programmes with `stop < now − max(epg_keep_past_days, max catch-up days)`
(chunked, FTS first); `epg::fts_cleanup`; history beyond 500 rows; image cache over 500 MB
(LRM); `PRAGMA optimize`; `wal_checkpoint(TRUNCATE)`.

---

## 8. Identity & reattach

### 8.1 Keys (`sources::identity`; MUST stay stable forever)
* Xtream: `xt:live:<stream_id>`, `xt:vod:<stream_id>`, `xt:series:<series_id>`, `xt:ep:<id>`.
* M3U: `m3u:` + 16 hex of blake3(`tvg_id` | `normalize_name(name)` | `normalize_name(group)`);
  duplicates inside one import get `#2`, `#3`… in file order. URLs are **not** part of the key
  (tokenized URLs rotate daily on many services).
* M3U series assembled from episode entries (phase 5, §27.5): `m3u:s:` + 16 hex of
  blake3(`normalize_name(series title)`); M3U movies/episodes keep their entry key (§27.4).
* Groups: `<kind>:<normalize_group(name)>` — a lighter normalization than `normalize_name`
  (country prefixes kept, so "UK | News" ≠ "US | News"); empty → `uncategorized`.
* The bodies of `sources::identity` are implemented in the skeleton and frozen (golden test);
  WP-03 owns the golden/property test files but not the bodies.

`normalize_name`: deunicode → lowercase → strip country/language prefixes (`us:`, `uk |`,
`|en|`, `[vip]`) and quality tags (`hd fhd uhd 4k sd 1080p 720p hevc h.265 (backup)`) → drop
punctuation/whitespace; **keeps `+1`/`+2`** (time-shifted channels are distinct).

### 8.2 What is keyed how
Catalogue rows: row id (stable while key stable). User data: `ItemRef` + JSON `Fingerprint`
(provider id, tvg-id, normalized name, normalized group = primary `grp.key` without the
`<kind>:` prefix, url key). **The store computes fingerprints** inside the writer closure
(`store::user::fingerprint_of`) — callers never pass one (UI summaries don't carry the facts).
Tables with `fp` + `orphaned_since`: `fav_item`, `item_override`, `watch_progress`,
`reminder`, `channel_epg_map` (manual rows). EPG mappings are keyed
`(source_id, channel_key, epg_source_id)`.

### 8.3 Reattach algorithm (`store::import::reattach_user_data`, inside `finish_catalogue`)
For every user-data row of the source whose key no longer exists:
1. same `provider_id` (Xtream sometimes keeps ids when re-categorizing);
2. same `tvg_id` **and** same `norm_name`;
3. same `norm_name` **and** same `norm_group`;
4. same `url_key` (scheme://host/path without query/credentials);
5. same `norm_name` alone if unique in the source.

First step with exactly **one** candidate wins → the row's `item_key` is rewritten (and
`orphaned_since` cleared). No unique candidate → `orphaned_since = now`; data is kept, shown
in Settings ▸ Orphans with "Attach to…" (manual `store::user::reassign`) and "Forget". Orphans
re-attach automatically if the item comes back. The summary (reattached/orphaned counts) is
shown in the refresh toast. Candidate lookups use indexes (`channel_provider`, `channel_tvg`,
`channel_norm`, `channel_urlkey`, `movie_provider`, `movie_urlkey`…) or HashMaps built once per
`finish_catalogue`. Tests cover: Xtream renumbering, renamed group, duplicate names (→ orphan,
never a wrong match), item removed then re-added, progress/reminder/manual-EPG reattach.

Known v1 limitation: `item_override.user_order` is per item, not per (group, item), so
reordering a channel inside one group also moves it relative to others in its other `;` groups.

---

## 9. Import pipelines

### 9.1 M3U (`sources::m3u` + `jobs::import_source`)
1. `HttpClient::download` to `~/.cache/tuner/playlists/<source>.m3u` (directory 0700, `.part`
   created 0600, + rename), conditional (ETag/Last-Modified → 304 ⇒ done, "not modified"),
   progress per 256 KiB; the file is deleted after a successful import (it carries credentials;
   the ETag stays in the DB); local files are read in place.
2. Sniff: magic bytes → gzip/xz decoder; payload kind → HTML (expired account / Cloudflare)
   = `Error::BadPayload` with copy "The server returned a web page instead of a playlist — the
   account may have expired"; HLS media playlist → offer "add as single stream".
3. `m3u::parse` on a blocking thread: BOM/CRLF/CR-only, state-machine tokenizer, header
   defaults (`url-tvg` → provider EPG sources, `tvg-shift`, catch-up defaults), `#EXTGRP`,
   `#EXTVLCOPT`, `#KODIPROP` (DRM ⇒ `unsupported = drm`), pipe headers, relative URLs.
4. Convert (smart names, keys via `KeyAllocator`, `;`-group lists, catch-up spec via
   `catchup::parse_kind`) and push
   batches of 2 000 through a bounded channel (cap 4) to the async writer loop → progress
   `Writing{done}`.
5. `finish_catalogue` → `ensure_provider_epg(url-tvg URLs)` → EPG refresh if stale →
   `RebuildLibrary` (follow-ups via `JobContext::submit`).

### 9.2 Xtream (`sources::xtream` + `jobs::import_source`)
`authenticate` (store `AccountInfo`; abort *before touching the catalogue* on AuthRejected /
Expired / Banned / Disabled; the outcome carries the account so the app sets the budget limit) → `get_live_categories` → `get_live_streams`
(single call, may be 100 MB; bytes → `serde_json::from_slice` in `spawn_blocking`) → write →
VOD categories/streams → series categories/series (each kind completes or fails
independently) → provider EPG (`xmltv.php`) → `RebuildLibrary`. Details are lazy:
`get_vod_info` / `get_series_info` when a detail page opens (re-fetched after 7 days).

### 9.3 XMLTV (`sources::xmltv` + `jobs::import_epg` + `store::epg`)
```
download (conditional, 0700 dir) → open_decompressed → [blocking] xmltv::parse ──bounded(4)──▶
[async] write_epg_channels (ON CONFLICT DO UPDATE; display-name norms computed by the importer)
        / write_programmes (5 000 rows/tx: INSERT … ON CONFLICT(channel,gen,start) DO NOTHING
        + their FTS rows) ──▶ finish_epg_import (short tx: flip active_gen, counts, max_stop,
resolve_mappings) ──▶ rematch (epg_match::match_all for every playlist source,
replace_auto_mappings, resolve_mappings) ──▶ sweep_old_generations (chunked, FTS first, then
unseen channels) ──▶ DataScope::Epg
```
Times: `YYYYMMDDhhmmss ±hhmm` (prefixes allowed; no zone ⇒ UTC; named zones), stored as UTC
seconds with the source offset applied (EPG time). Channel `tvg-shift` is applied at read time,
exactly once: every store read returns EPG time and UI callers apply
`ProgrammeSlim::shifted(tvg_shift_min)` (rows, grid, OSD, search hits); the exception is
`store::epg::now_next(conn, &[(epg_channel, shift)], now)`, which queries at `now − shift` and
returns shifted results keyed by (channel, shift). `ProgrammeCache` keys buckets by (epg
channel, 6 h bucket) in EPG time and shifts after the lookup. Sign: displayed = EPG + shift
(`tvg-shift="1"` → +60 min). Catch-up converts back (§10). Parser options skip programmes
ending before `now − keep_past` and starting after `now + 14 d`. Memory stays bounded by
back-pressure. Budget: 415k programmes end-to-end < 8 s.

### 9.4 EPG channel matching (`sources::epg_match`)
Priority (first hit wins; manual mappings are never overwritten):
1. `tvg-id == xmltv id` (exact), then case-insensitive;
2. Xtream `epg_channel_id` against the same source's provider guide (tagged `XtreamEpgId`);
3. iptv-org ids: strip `@Feed` on either side;
4. `normalize_name(tvg-name or name)` equals a normalized display-name, **only if unique** in
   that EPG source (config `epg.auto_name_match`);
5. fuzzy (Jaro-Winkler ≥ `epg.fuzzy_threshold` = 0.92) → *suggestions only* in the EPG
   mapping screen; accepting stores `Manual`. Suggestions are computed lazily for VISIBLE rows
   on a worker with a token/trigram prefilter (a full unmapped × EPG scan would be ~1.5e9
   comparisons at scale); the "Has suggestions" filter uses the prefilter's cached results.

Resolution per channel (`resolve_mappings`): Manual > lowest `epg_source.priority` > best
method, among enabled sources whose epg channel exists → `channel_epg`. Ordered EPG sources
and per-source offsets are Settings ▸ EPG. The **EPG mapping screen** lists channels filtered
by Unmapped / Has suggestions / All, shows suggestions + a searchable list of all EPG channels;
key contexts `[EpgMapping, List|ListFiltering]`: Enter assigns, Delete clears (Ctrl+E jumps
there for the focused channel).

### 9.5 Refresh scheduling & staleness
* On launch: anything older than `refresh_on_launch_after_h` (12 h) → `RefreshStale` after 5 s.
* While running: tick every 15 min → playlists older than `refresh_h` (source) /
  `playlist_refresh_h` (24 h); EPG older than `epg_refresh_h` (12 h) **or whose data ends
  within 12 h** (`max_stop`); skipped while the player is Loading/Zapping
  (`JobContext::player_busy`; don't compete with a zap).
* Daily: `Maintenance`.
* Staleness UI: status bar "Guide updated 3 h ago" (warning glyph when > 48 h or `max_stop`
  passed: "Guide data ended — refresh"); per-source "Updated … / Failed … (reason)" in
  Settings; channel rows show "No guide data" rather than empty space ("Loading guide… n %"
  during the first guide import).
* Closed app: no background refresh in v1. `tuner --refresh` (for a future user timer) refreshes
  stale sources headlessly and exits (status 1 if a job failed). *Decided in phase 2:* while a
  windowed instance is running it does nothing and exits 0 — that instance owns the database
  writer and its scheduler refreshes on its own. *Phase 5 audit:* it holds the single-instance
  lock (§21.4) for the whole refresh, marked `refresh` in the lock file, so the database keeps one
  owner: a window launched meanwhile waits as for a starting instance (10 s) and then exits
  with "a background refresh (tuner --refresh) is running; start Tuner again once it has
  finished", on stderr and as a desktop notification (a launcher has no terminal). Upgrades it
  performs are toasted by the next window (§27.3).

---

## 10. Catch-up URL building (`sources::catchup`, `sources::resolve`)

Availability: Xtream `tv_archive=1` (days = `tv_archive_duration`) or M3U catch-up attributes;
`in_window(spec, start, now)` ⇔ `now − days·86400 ≤ start < now`. The guide shows ⟲ on such
past programmes; Enter → "Catch up".

Effective start = programme start − `catchup-correction` − `tvg-shift`·60 + per-channel user
offset (Ctrl+I "Edit information", UHF 1.100.1). The programme start/stop passed in are the
DISPLAYED (shifted) times; subtracting the shift converts back to the archive's EPG time. Duration = stop − start (3600 if
unknown), capped at `now`. Granularity: 1 s if the template uses `{utc}`, `${start}`, `{S}`,
`{offset:1}`, else 60 s.

| Kind | URL built | Example (live `http://h/ch/index.m3u8`, start 2026-10-09 20:00 UTC, 60 min) |
|---|---|---|
| Default | expand(`catchup-source`) (absent ⇒ Append) | `http://h/archive?start={utc}&end={utcend}` → `…start=1791576000&end=1791579600` |
| Append | live URL + expand(`catchup-source`) | `?utc={utc}&lutc={lutc}` appended |
| Shift / timeshift | live URL + `?`/`&` + `utc={utc}&lutc={lutc}` | `http://h/ch/index.m3u8?utc=1791576000&lutc=…` |
| Flussonic (fs, -hls, -ts) | rewrite: `…/mpegts` → `…/timeshift_abs-{utc}.ts`; `…/index.m3u8` → `…/timeshift_rel-{offset:1}.m3u8`; `…/mono.m3u8` → `…/mono-timeshift_rel-{offset:1}.m3u8` | `http://h/ch/timeshift_rel-5400.m3u8` |
| Xc (M3U `xc`/`xtream`) | parse `host/(live/)?u/p/id(.ext)` or `host/path/live/u/p/id(.ext)` → `host(/path)/timeshift/u/p/{duration:60}/{Y}-{m}-{d}:{H}-{M}/id.ext` | `/timeshift/u/p/60/2026-10-09:21-00/1234.ts` (server tz Europe/London, BST) |
| Native Xtream source | `xtream::urls::timeshift_url(creds, stream_id, start_in_server_tz, ceil(min), ext)` | same as Xc; tz = source override → `AccountInfo.server_tz` → local |
| Vod | expand(`catchup-source`) or `{catchup-id}` | — |

Placeholders (Kodi semantics, re-implemented): `{utc}` `${start}` `{utcend}` `${end}` `{lutc}`
`${now}` `${timestamp}` `{duration}` `${duration}` `{duration:X}` `{offset:X}` `${offset}`
`{Y}{m}{d}{H}{M}{S}` (server/local tz) and formatted `{utc:FMT}` `${start:FMT}` `{utcend:FMT}`
`${end:FMT}` `{lutc:FMT}` `${now:FMT}` (Y m d H M S → strftime), `{catchup-id}`. Unknown tokens
stay verbatim. Catch-up plays as `PlayableKind::Catchup` (seekable; `L` jumps to live).

---

## 11. Player

### 11.1 Rendering (implemented, `player/render.rs`)
Spike recipe (D1). `VideoSurface::render` runs in `App::logic` (GL current). Size requested by
the layout in the previous frame (one-frame resize lag, invisible). The FBO covers the whole
video rect and mpv letterboxes (so aspect/panscan/zoom are mpv's job). Until a screen places the
video, frames go to a 640×360 sink (`mpv_render_context_update` alone does not consume frames;
CLI `--play`, MPRIS and Compact without a mini-player would otherwise stall the VO). On a new
load the FBO is cleared to black. Teardown order: render ctx → FBO → mpv (explicit
`MpvGl::destroy` with GL current; a plain drop leaks, see §5.1).

**Frame pacing and hidden windows (corrected twice).** The window's frames are paced by
Wayland frame callbacks, never by a blocking swap:
* `main` sets `glow_options.vsync = false` (swap interval 0). With eframe's default (1), Mesa's
  `eglSwapBuffers` waits for the frame callback of the *previous* frame
  (`dri2_wl_swap_buffers_with_damage` dispatches its queue until the throttle callback fires).
  Hyprland withholds frame callbacks from windows on hidden workspaces, so the first swap after
  the workspace was left blocked the UI thread indefinitely: winit no longer dispatched the
  Wayland socket, `xdg_wm_base` pings went unanswered, and after `misc:anr_missed_pings`
  Hyprland put up its "application not responding" dialog — while mpv kept playing on its
  own threads.
* `App::ui` calls `winit::Window::pre_present_notify()` (eframe 0.36 never does —
  `native/glow_integration.rs` only swaps). winit-wayland then requests a `wl_surface.frame`
  with the commit of eframe's swap and withholds the next `RedrawRequested` until that callback
  arrives (`platform_impl/linux/wayland/event_loop/mod.rs`: `FrameCallbackState::Requested`
  → no redraw). Repaint requests (mpv's update callback, timers) therefore coalesce to one
  frame per compositor frame — the same cadence vsync gave, without blocking, and no spin:
  with nothing to repaint the loop sleeps in `epoll`. eframe runs `ui` only for frames it
  presents (an invisible root viewport gets `logic` alone), so a requested callback always
  rides on a commit.
* Hidden: no frame callback → no `RedrawRequested` → neither `logic` nor `ui` runs, and the UI
  thread waits in `epoll` on the Wayland fd and answers pings. Hyprland sends no frame
  callbacks there unless the window has the `render_unfocused` rule — shipped in
  `packaging/hypr/tuner.lua` (callbacks at `misc:render_unfocused_fps`, default 15). Without
  the rule nothing renders, mpv's VO waits ~200 ms per frame and drops it, and decoding burns
  CPU for nothing. Safety net: every render stamps the
player's `RenderClock`; the controller tick sets `vid=no` after 2 s without a rendered frame
while Playing (audio continues, `snapshot.video_suspended = true`), and the app sends
`PlayerCommand::ResumeVideo` (`vid=auto`) as soon as `logic` runs again. Nothing else that must
work while hidden lives on the UI thread (§3.1). **Verification (WP-07, repeated by WP-13)**:
60 s on another workspace with and without the rule — audio continuity, `frame-drop-count`,
CPU, video resumes; fullscreen toggling; 1-hour soak; HDR/10-bit sample.

### 11.2 Event pump & controller (implemented)
`events::EventPump` observes properties (pause, paused-for-cache, cache-buffering-state,
core-idle, time-pos, duration, demuxer-cache-duration, volume, mute, track-list (JSON), seekable,
sub-delay, sub-scale, video-params/w,h, hwdec-current, aid, sid, eof-reached), requests `warn`
log messages (redacted; `HTTP error NNN` is parsed for classification) and forwards owned
`MpvEvent`s. The controller (`player/controller.rs`) multiplexes commands, events and a 250 ms
tick, feeds `PlayerMachine`, executes `Effect`s (load, stop, emit, save progress, push history,
record/clear failure — persistence through the injected `PlayerStore`), applies direct commands
(pause, seek, volume, tracks, sub delay/scale, aspect, zoom, `ApplyConfig`, `ResumeVideo`),
resolves media through the injected `MediaResolver` (tests use fakes for both), and publishes
`Arc<PlayerSnapshot>`: state changes and command results immediately **with a repaint
request**; continuous values ≤ 10 Hz; time-pos repaints only when the whole second changes.
`TracksChanged` is emitted only when the parsed track list actually changes (`track-list`
re-fires on `demux-bitrate` etc.). The snapshot carries `origin` (where playback was started;
zaps keep it) and `video_suspended`.

### 11.3 State machine (`player/state.rs`, pure, unit-tested)

```
            Play(delay)               tick ≥ until
 Idle ───────────────────▶ Zapping ───────────────▶ Loading ◀──────────────────┐
  ▲    Play(immediate) ─────────────────────────────▶  │ PlaybackRestart        │ tick ≥ at
  │                                                    ▼ (after START_FILE of   │
  │  Stop               Pause(true)              ┌── Playing ──┐  our entry) Reconnecting
  ├──────────────────── Paused ◀────────────────▶│             │ ───────────▲   │ policy says stop
  │                                              └─ Buffering ◀┘ paused-for-cache   ▼
  │                       EndFile EOF (VOD) ───▶ Ended                            Error ──Retry──▶ Loading
```
Failures: END_FILE error/EOF-on-live; Loading > `network_timeout_s + 5`; Buffering > 15 s
(live) / 30 s (VOD); live time-pos frozen 12 s while Playing (stall).
**Entry tracking**: after every `loadfile` the controller reads the new entry id
(`playlist/0/id`; synchronous, the controller is the only playlist writer) and feeds
`Input::Loaded{entry}`. START_FILE/END_FILE are matched against that id; PLAYBACK_RESTART is
accepted only after START_FILE of the expected entry. So "Load A, Load B, START(A), END(A,
stop), START(B), END(B, error)" reconnects B, and a late restart of A never marks B Playing
(unit test `fast_switch_binds_events_to_the_right_entry`).
**Resolve failures** (`Input::ResolveFailed`) never touch the current target: what plays keeps
playing (Idle → Error state), the UI gets `PlayerEvent::Failed` (toast / OSD), nothing is written
to the failure cache, and the resolver remembers the channel so zapping skips it.

### 11.4 Reconnect policy (`player/reconnect.rs`)

| Failure | Live | VOD / catch-up |
|---|---|---|
| Network, Timeout, Stalled, StreamEnded (EOF on live), HTTP 5xx, other mpv | backoff 0.5 · 1 · 2 · 4 · 8 · 15 · 15… s, max `reconnect_max_attempts` (8) | backoff, max 3, **resume at last position − 2 s** |
| HTTP 401/403 | 1 retry after 3 s (connection-limit race right after a zap), then Error "account expired or too many connections" | same |
| HTTP 404/410 | 1 retry after 2 s, then Error "channel offline" | Error |
| Other 4xx, unsupported format, DRM, resolve error, budget | Error immediately | Error |

HTTP statuses (parsed from mpv/ffmpeg log lines) classify a failure only while **Loading**
(they are cleared on PlaybackRestart): a storm of HLS segment 404s while Playing is a Network
failure (backoff), not "channel offline". Attempts reset after 30 s of stable playback. mpv's `stream-lavf-o reconnect*` covers TCP
drops inside a file; the policy covers everything that ends/stalls the file (HLS segment 404
storms, token expiry, server kicks). Overlay: "Reconnecting… (2/8)" with countdown; `r` retries
now; Backspace = last channel.

### 11.5 Zapping & connection budget
* **One stream at a time.** Every load releases the previous lease. **Stop-before-start**:
  `loadfile … replace` returns BEFORE the old file is stopped and demuxer teardown is
  asynchronous, so when the old and new streams share a source whose limit is ≤ 1 or unknown,
  the controller sends `stop`, enters `PendingLoad`, and issues `loadfile` on the old entry's
  END_FILE (or after 1.5 s at most). Leases are released on END_FILE, never before the command.
  (With `max_connections = 1`, the usual Xtream limit, a direct replace raced into a 403 and a
  3 s retry.)
* **Zapping delay** (`zap_delay_ms`, default 300): Up/Down and numeric entry enter `Zapping`;
  repeated zaps inside the window only move the target (the info banner shows the new channel
  instantly); the connection is made once the user pauses. Enter/explicit play is immediate.
* **Zapping skips** channels for which `MediaResolver::skip_on_zap` is true (≥ 2 failures in
  24 h via `store::live::should_skip`, DRM, resolve failure this session) using
  `LiveIndex::neighbour` semantics.
* **Preview autoplay** (Live, `live_preview_autoplay`) is **off by default** and, when on, uses
  its own `preview_delay_ms` (1200), triggers only from keyboard cursor rest (never hover), and
  only when nothing plays or the current stream was itself started by preview
  (`snapshot.origin == LivePreview`). Guide preview is off by default for the same reason.
* **Budget** (`player/budget.rs`): `max_connections` per source from `AccountInfo`, sent with
  `SetConnectionLimit` at startup (`store::sources::account_limits`) and after every refresh;
  v1 never exceeds 1 open lease, and any future feature (multiview, recording, guide preview in
  a second instance) must acquire a lease and is refused with a clear message at the limit.
* If `active_cons ≥ max_connections` at login, Settings shows a warning ("another device is
  using this account").
* **Settings apply live**: `PlayerCommand::ApplyConfig` re-derives `MachineConfig` (zap delay,
  timeouts, reconnect attempts, watched %) and sets `hwdec`, `cache-secs`,
  `demuxer-readahead-secs`, `demuxer-max-bytes`, `network-timeout`, `alang`, `slang`,
  subtitle auto mode, `sub-scale`, user agent. No v1 setting needs a restart (raw
  `mpv_options` apply from the next file).

### 11.6 Failure cache
`stream_failure` rows per channel (reason, count, time). Rows with ≥ 2 failures in 24 h are
dimmed with a warning glyph; zapping skips them (§11.5); a successful play clears the row.
Critical because many free playlists are mostly dead (spike: 38 of 40 failed).

### 11.7 mpv options
Init (`player/mpv.rs`): `vo=libmpv`, hwdec (D1), `idle=yes`, `keep-open=no` (live EOF must
end the file), `config=no`, no OSC/input/ytdl, `video-timing-offset=0`, cache on,
`demuxer-readahead-secs`/`cache-secs` = `cache_secs` (1 when instant load), `demuxer-max-bytes`,
`demuxer-max-back-bytes=32MiB`, `network-timeout`, `stream-lavf-o=reconnect=1,
reconnect_streamed=1,reconnect_on_network_error=1,reconnect_delay_max=5`, `user-agent`,
`alang`/`slang`, subtitle auto mode, `sub-scale`, `volume`, `mute`, `audio-client-name=tuner`,
then user `player.mpv_options`. Per file: `loadfile <url> replace -1
force-media-title=%n%…,user-agent=…,referrer=…,http-header-fields=…,start=…` (values `%len%`
quoted). The title never comes from the URL (credentials).

### 11.8 Frame pacing (known limitation)
UI frames follow the compositor's frame callbacks (§11.1: vsync off, `pre_present_notify`), so
mpv's update callback is served once per display refresh at most, aligned to the compositor's
repaint like the old blocking vsync swap but without blocking the UI thread.
Without a post-swap hook `report_swap` is omitted (it halved throughput in the spike) and the
panel is fixed 60 Hz, so 25/50 fps content shows cadence judder; `video-sync=audio` (default)
keeps A/V sync. WP-07 records drops/s and `vo-delayed-frame-count` for 25/30/50/60 fps samples;
users may experiment via `mpv_options` (`video-sync=display-resample`, `interpolation`).

---

## 12. Layout

### 12.1 Breakpoints (`ui/layout.rs`, logical px × rem)

`rem = [font] base-size / 12 × interface.ui_scale` (`Theme::rem`). Breakpoints, sidebar widths
and guide geometry are multiplied by it, so `omarchy display text size 16` (rem 1.33) turns a
790 pt tiled half into Compact instead of cramming Medium.

| Class | Width (rem 1) | Typical | Sidebar (Auto) | Live | Guide | VOD grid |
|---|---|---|---|---|---|---|
| Compact | < 700 | quarter tile, small floating | 48 rail down to 480; below: top glyph strip (WP-10a) | stack: groups → channels → fullscreen | logo-only column, 2.5 px/min | 2–3 cols |
| Medium | 700–1199 | tiled half (≈ 790) | icon rail 48 | channels + preview on the right (groups in a drawer) | 140 column, 3 px/min (≈ 3.5 h) | 4–5 cols |
| Wide | ≥ 1200 | full (≈ 1600) | expanded 184 | groups + channels + preview | 200 column, 4 px/min (≈ 5 h) | 7–9 cols |

`with_inner_size` is ignored when tiled — layouts must work at any size ≥ 360×240. The
fullscreen player and onboarding have no sidebar/status bar. OSD scale `s` =
clamp(width/1280, 1, 1.6).

### 12.2 Shell
```
┌sidebar┬───────────────────────────── content (screen) ─────────────────────────────┐
│ 󰋜     │                                                                             │
│ 󰔂     │                                                       ┌ toast ────────────┐ │
│ 󰃰     │                                                       └───────────────────┘ │
│ 󰎁     │                                                       ┌ mini player ──────┐ │
│ 󰟴     │                                                       │  video 16:9       │ │
│ 󰍉     │                                                       └ BBC One · ✕ ──────┘ │
│ 󰒓     │                                                                             │
├───────┴─────────────────────────────────────────────────────────────────────────────┤
│ ↵ play · ⇧↵ fullscreen · Ctrl+F fav · Shift+F10 more · ? help   Guide updated 2 h ago│  ← status bar 26·rem
└─────────────────────────────────────────────────────────────────────────────────────┘
```
Content padding = panel-padding (18). Panes separated by 1 px `pane_divider` (fg @ 0.28).
Status bar: left = running jobs ("󰑐 Refreshing My IPTV · 45 %", click = Ctrl+J) or, when idle,
the per-context hint line generated from the keymap (`keymap::hint_line`); right = guide
freshness, offline marker, and — when the mini-player is hidden (Compact or disabled) — a
now-playing chip (click = player, ✕ = stop). **No clock** (the Omarchy bar already shows one).
Toasts stack bottom-right ABOVE the mini-player rect (`VideoHost::mini_rect`), which screens
also receive as `UiCtx::mini_rect` (the guide keeps the focused cell clear of it). The
mini-player never shows over Onboarding or the Player (`allows_mini_player() == false`).
Overlays centre with a bg @ 0.5 scrim.

### 12.3 Video-only layout (pop-out, `ui/popout.rs`)
The window shows only the player — no sidebar, status bar or mini player — while
* our pop-out is requested or active (`o` / Ctrl+Shift+O, §21.6), whatever its size, or
* Hyprland reports the window **floating and pinned** and it is at most Omarchy's Super+O pop
  size, 1300 × 900 logical px (`omarchy-hyprland-window-pop`'s default). A bigger pinned
  window keeps the full UI; floating alone never switches.

It never shows while a pop-in is in flight, nor between the pop-in's result and Hyprland's
unpin event (no flash). The app draws the Player screen and gives it the keys whatever the
route stack says (`TunerApp::view_route`); the stack itself stays, so popping back in returns
to the screen the user left (it is entered again). In video-only mode a route step back
(`p` keep playing, `q` stop and close, a finished movie returning to its page) pops the window
back in instead of leaving the player. Key stack: the player's contexts with `PopOut` right
above `PlayerLive`/`PlayerVod` — Esc pops back in once a side panel, number entry or controls
focus has had it.

---

## 13. Screens & components

### 13.0 Playback rules shared by every screen
* **Enter rule.** Enter on the channel that is already playing opens the fullscreen player.
  Otherwise Enter plays *here* where a preview pane is visible (Live Wide/Medium), and
  fullscreen everywhere else (Home, Search, Compact, the guide's Watch). **Shift+Enter is always
  fullscreen.** (uhf.md §15 "Enter goes fullscreen" holds wherever there is no preview.)
* **Zap collection** (what ↑/↓ walk in the player): the list/group the channel was played from.
  From Search or Home: the channel's primary group — except the Home favourites rail, which
  zaps through that favourites list. `Collection::Adhoc` is only for explicit lists.
* **Return from the player**: the Live list cursor (and the guide focus) follow the channel that
  was zapped to.
* **Pane focus & now playing**: only the focused pane shows the cursor fill; other panes show
  their current row with the "selected" fill (fg @ 0.18). The playing channel carries an accent
  󰐊 glyph in every list.
* **Hover** moves list cursors only after real pointer motion (Omarchy `Menu.qml`
  `pointerGate.moved()`), never because rows scroll under a still pointer, and never triggers
  playback.
* **Reorder vs sort**: Alt+↑/↓ while the list is sorted by anything other than *Custom order*
  (`SortMode::Provider` = provider order with user reordering) switches it to Custom order with
  a toast "Switched to custom order". Ctrl+S opens a small sort & view menu (launcher card),
  never a blind cycle.

### 13.1 Component catalogue (Omarchy tokens → widgets)

| Component | Widget | Tokens |
|---|---|---|
| Menu/list row | `widgets::rows::menu_row`, `list::virtual_list` | height max(50, body + 2·row-padding-x), 58 with detail; label heading 16 Medium; detail body-small 11 @ 0.52; icon column 36 @ icon-large 18; chevron @ 0.36; cursor = fg @ 0.08 fill + **accent** text (+ optional 1 px fg @ 0.25); selected = fg @ 0.18 |
| Channel row | `rows::channel_row` | as menu row; number body @ 0.52; progress bar 3 px (track fg @ 0.18, fill fg); next programme body-small @ 0.52 |
| Section header | `section::section_header` | caption 10, bold, darker(fg, 1.4), top pad ceil(0.15·size) |
| Separator / pane divider / group divider | `section::*` | 1 px fg @ 0.12 / 0.28 / 0.20 |
| Card (menu / launcher) | `card::card(Menu)` | bg `menu.background`, 2 px `menu.border` (fg), padding 18 |
| Popup card | `card::card(Popup)` | bg `popups.background`, 2 px accent border, padding 14, offset 5 |
| Tooltip | `card::card(Tooltip)` | bg @ 0.97, 1 px fg, body-small, padding 10×6, 400 ms delay |
| Button | `button::button` | borderless at rest; padding 10×6; gap 8; hover fg @ 0.08 + 1 px fg @ 0.25; pressed fg @ 0.22; selected accent + bold; primary = accent text, bordered |
| Text field / search header | `text_field::*` | padding 10×7; field fill fg @ 0.04 + 1 px fg @ 0.4; focus fg @ 0.08 + 1 px fg @ 0.25; placeholder darker(fg,1.6) / @ 0.58 in launcher headers (heading 16); selection fg @ 0.35 |
| Toggle | `toggle::toggle` | track max(22, 0.55·control-height) × 1.9; checked = fg @ 0.18 + accent knob; unchecked fg @ 0.04 + darker(fg,1.25) knob; square |
| Segmented | `toggle::segmented` | buttons; selected = fg @ 0.18 + accent bold |
| Slider / seek bar | `slider::*` | track max(4, 0.11·control-height) fg @ 0.18, fill fg, knob max(14, 0.38·ch) ×1.15 hover; seek bar over video uses OSD colours |
| Progress | `progress::*` | programme progress fg; watch progress accent; busy bar for unknown totals |
| Badge | `badge::*` | caption, 1 px border colour @ 0.6; LIVE = urgent; source = source colour (palette role) + glyph |
| Logo / monogram | `logo::logo` | square, aspect kept; monogram on fg @ 0.04, text @ 0.52 |
| Poster card | `poster::poster_card` | 2:3; title body; meta body-small @ 0.52; progress accent; cursor 2 px accent outline |
| Empty / error state | `empty_state::empty_state` | glyph display-large 28 accent @ 0.8; title 14 fg @ 0.7; detail @ 0.52 |
| Sidebar | `sidebar::sidebar` | bg dark_background; row control-height + md; selected fg @ 0.18 + accent |
| Status bar | `status_bar::status_bar` | height 26 (Omarchy bar), body-small, fg @ 0.52 |
| Toast | `toast::Toasts` | popup card, kind glyph (info fg, success green, warning yellow, error urgent) |
| Scrim | overlays | `menu.scrim` @ 0.5; click cancels |
| Video OSD | player screen | fixed `rgba(0,0,0,170)` scrim, text #f2f2f2, `tokens.osd_accent` for focus/progress/seek bar (theme accent if its contrast on the scrim over mid-grey video is ≥ 3:1, else #f2f2f2 — e.g. the `white` theme's #6e6e6e); type: title = Display 24·s, body = Title 14·s, badges = Caption·s (s = `osd_scale`) |
| Now playing marker | rows, tiles | accent 󰐊 glyph left of the name |
| Status hint line | status bar | body-small @ 0.52, generated from the keymap (§12.2) |
| Drag grip | reorderable rows (Settings: playlists, EPG sources, favourites lists) | 󰇙 @ 0.36 at the row start; drag to reorder, Alt+↑/↓ or K/J by keyboard |
| Motion | all | colour transitions 100–120 ms, popups 140 ms ease-out; none longer |

Rounding: Hyprland `decoration:rounding` (0 on Omarchy) everywhere; no shadows, no blur.

### 13.2 Live TV
Wide (≈ 1600):
```
┌────────┬────────────────────────┬────────────────────────────────────────┬───────────────────────────────────────┐
│ 󰋜 Home │ GROUPS            ⌕    │ UK News…                               │ ┌───────────────────────────────────┐ │
│ 󰔂 Live │ 󰓎 Favourites       42  │ ▣ 101 BBC One HD              󰓎 ▕██░▏ │ │                                   │ │
│ 󰃰 Guide│ 󰓎 Sport (list)     11  │     News at Eight · 20:00–21:00        │ │          VIDEO  (preview)         │ │
│ 󰎁 Movie│ 󰋚 History          12  │ ▣ 102 BBC Two                    ▕█░░▏ │ │                                   │ │
│ 󰟴 Serie│ All channels    9 812  │     Gardeners' World · next Panorama   │ └───────────────────────────────────┘ │
│        │ ───────────────────────│ ▣ 103 ITV1  󰀦 (offline, dimmed)        │ BBC One HD · 101   [HD] [⟲ 7 d] [▣ Pr]│
│ 󰍉      │ UK News           128  │ ▣ 104 Channel 4                        │ News at Eight          20:00 – 21:00  │
│ 󰒓      │ UK Sports          64  │ …                                      │ ▕█████████░░░░░░▏ 34 min left          │
│        │ Kids 󰌾             31  │                                        │ The latest national and international │
│        │ …                      │                                        │ news, followed by weather.            │
│        │                        │                                        │ UPCOMING                              │
│        │                        │                                        │ 21:00  Drama                          │
│        │                        │                                        │ 22:00  Newsnight                   󰂚  │
├────────┴────────────────────────┴────────────────────────────────────────┴───────────────────────────────────────┤
│ status bar                                                                                                       │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```
Groups 240·rem · channels flex (≥ 380) · preview 38 % (≥ 420). The channel pane header *is*
the filter (launcher idiom; haystack `"<number> <name>"`, so typing `101` finds channel 101
first). Virtual groups on top: each favourites list, History, Reminders, All channels; then
provider groups (renamed/reordered/hidden per overrides; locked show 󰌾 and require PIN).
* Enter follows §13.0 (plays in the preview; on the playing channel → fullscreen);
  Shift+Enter fullscreen; Ctrl+G opens it in the guide; Ctrl+I "Edit information".
* **Preview autoplay is OFF by default** (each tune costs a provider connection and, on
  Asahi, software decode). When enabled: only after the keyboard cursor rests
  `preview_delay_ms` (1200), never from hover, and only if nothing is playing or the current
  stream was itself started by autoplay — it never replaces a VOD or a channel the user started.
* **Favourites lists** are managed from the groups pane: the context menu (Shift+F10 / right
  click) on a list row offers Rename (F2), Delete (Confirm; the default list can't be deleted)
  and reorder (Alt+↑/↓); new lists come from Ctrl+Shift+F "Favourites lists… → New list…" or
  Settings ▸ Favourites.
* **Reminders** virtual group: channels with pending reminders, programme + time; Delete
  cancels (also Settings ▸ Reminders).
* Reorder (Alt+↑/↓) inside a provider group writes `item_override.user_order` (§8 limitation
  for `;` multi-group channels).

Medium (≈ 790): the preview stays on the RIGHT; groups become a drawer (← / Backspace on the
channel list, or click the breadcrumb):
```
┌──┬────────────────────────────────┬─────────────────────────────┐
│󰋜 │ UK News ▾  (filter…)           │ ┌─────────────────────────┐ │
│󰔂 │ ▣ 101 BBC One HD      ▕██░▏    │ │      VIDEO (16:9)       │ │
│󰃰 │   News at Eight               │ └─────────────────────────┘ │
│󰎁 │ ▣ 102 BBC Two         ▕█░░▏    │ BBC One HD · 101 [HD]       │
│󰟴 │   Gardeners' World            │ News at Eight  20:00–21:00  │
│󰍉 │ …                             │ ▕██████░░░▏                  │
│󰒓 │                               │ 21:00 Drama · 22:00 Newsnight│
└──┴────────────────────────────────┴─────────────────────────────┘
```
Compact: single column stack (groups → channels); Enter plays fullscreen; preview hidden; the
status bar shows the now-playing chip.

### 13.3 Home
```
┌────────┬──────────────────────────────────────────────────────────────────────────────────┐
│sidebar │ CONTINUE WATCHING                                                                │
│        │ [poster▁▁▁] [poster▁▁ ] [poster▁  ] [episode S2E5▁▁]   →                          │
│        │ FAVOURITE CHANNELS                                                               │
│        │ [▣ BBC One  News at Eight ▕██░▏] [▣ CNN  World Report ▕█░░▏] [▣ ESPN Live: …]    │
│        │ RECENTLY WATCHED                                                                 │
│        │ [▣ ITV1] [▣ Sky News] [▣ Eurosport 1] …                                          │
│        │ RECENTLY ADDED MOVIES                    RECENTLY ADDED SERIES                    │
│        │ [poster] [poster] [poster] …             [poster] [poster] …                     │
└────────┴──────────────────────────────────────────────────────────────────────────────────┘
```
Key context `[PosterGrid]`: ↑/↓ between rails, ←/→ within a rail (letters do not filter on
Home). Enter plays channels FULLSCREEN (§13.0; zap collection = the rail's favourites list or
the channel's primary group) and opens VOD details; Shift+Enter resumes VOD directly. One
favourites rail per non-empty list. Empty rails are omitted; when fewer than 2 rails remain a
**"Get started" rail** appears: Browse Live TV · Open guide · Add favourites (Ctrl+F) · the
first group's channels with now/next. An empty library routes to onboarding. The default start
screen is `StartScreen::Auto`: Live until the user has favourites or history, then Home.
Medium: rails narrower, same structure.

### 13.4 TV Guide & EPG grid widget
```
┌────────┬────────────────────────────────────────────────────────────────────────────────────────────┐
│sidebar │ TV GUIDE  [Favourites ▾]  [‹] Thu 9 Oct [›]  [Now]       f collection · [ ] day · n now   │
│        ├────────────────────────┬─────────────┬────────────┃┬─────────────┬─────────────┬──────────┤
│        │                        │ 19:30       │ 20:00      ┃│ 20:30       │ 21:00       │ 21:30    │
│        ├────────────────────────┼─────────────┴──────┬─────┃┴─────────────┼─────────────┴──────────┤
│        │ ▣ 101 BBC One          │ ⟲ Quiz             │ News at Eight ▒▒▒▒▒▒│ Drama                  │
│        │ ▣ 102 BBC Two          │ ⟲ Gardeners' World      ┃ │ Panorama      │ Film: The Matrix  󰂚     │
│        │ ▣ 103 ITV1             │ No information           ┃                                        │
│        │ ▣ 104 Channel 4        │ ⟲ The Simpsons │ Hollyoaks┃│ ┌Big Bang Theory (focus)┐ │ Gogglebox │
│        │ …                      │                          ┃                                         │
└────────┴────────────────────────┴──────────────────────────┸─────────────────────────────────────────┘
                                                        now-line (2 px accent)
```
* **Geometry** (`GridMetrics::for_layout(layout, compact, &theme)`, all × rem): sticky ruler
  (= control-height) with 30-min ticks and day label; sticky channel column (Wide 200 /
  Medium 140 / Compact 56); rows max(48·rem, 1.5·control-height) (compact density 36·rem);
  `px_per_min` 4 / 3 / 2.5 × rem.
* **Collection**: header button "Favourites ▾" (click or `f`) picks favourites lists, groups or
  All. Default: the first non-empty favourites list, else the last-used group. ‹ › and Now are
  clickable buttons too.
* **2-D virtualization**: paint/query only rows in view ± 2 and times in view ± 1 h.
  `ProgrammeCache` keys `(epg_channel, 6 h bucket)` in EPG time (tvg-shift applied after the
  lookup); misses load synchronously while the frame's 2 ms DB budget allows (≤ 24 buckets),
  the rest next frame with skeleton cells and a repaint request; LRU 4 000 buckets; full
  invalidation on `DataScope::Epg`. All reads go through `&mut dyn EpgReader` (fake in tests).
* **Cells**: title (body, clipped with …), time (body-small @ 0.52) if ≥ 60 px; past @ 0.52;
  airing gets fg @ 0.08 fill up to now; ⟲ when catch-up available; 󰂚 when a reminder is set;
  gaps = one "No information" cell (@ 0.36); focus = list-cursor style.
* **Now-line**: 2 px accent across rows + ruler marker; `request_repaint_after(30 s)`.
* **Keyboard focus model**: a persistent *focus time* T. ↑/↓ keep T and focus the programme
  covering T on the new row (nearest if none). ←/→ focus the previous/next programme and set
  T = max(start, view_start). The view scrolls to keep the focused cell visible (its start if
  longer than the view) and keeps 2 rows of margin. PgUp/PgDn = visible rows; `[`/`]` ± 24 h;
  Shift+←/→ ± 3 h; `n`/Home = now; `g`/`G` first/last channel; **digits** open numeric entry
  (`[NumericEntry, Guide]`) and jump to that channel number. Focus/scroll targets keep clear of
  the mini-player rect.
* **Mouse**: wheel = rows, Shift+wheel / horizontal trackpad = time, click = focus,
  double-click = details, click on the channel column = watch, hover tooltip (title, time,
  description excerpt).
* **Actions**: Enter → programme popover (Watch if airing · Catch up if past & archive ·
  Remind me / Cancel reminder if future · Assign EPG); `w` watch, `c` catch-up, `r` toggles a
  reminder; `f` collection picker; Ctrl+E assign EPG. Returning from the player re-focuses the channel just
  watched (UHF 1.96). Optional preview strip (Wide, `guide_preview`).
* **Perf**: < 8 ms/frame while holding ↓ over 5 000 channels — measured by `benches/guide.rs`
  (release, headless `egui::Context`, fake `EpgReader`, median of 200 frames).

### 13.5 Movies / Series
```
┌────────┬──────────────────────┬──────────────────────────────────────────────────────────────────┐
│sidebar │ CATEGORIES      ⌕    │ Action…  (type to filter)       Sort: Recently added ▾ (Ctrl+S) │
│        │ All movies   48 211  │ ┌────┐ ┌────┐ ┌────┐ ┌────┐ ┌────┐ ┌────┐ ┌────┐                     │
│        │ Continue watching    │ │    │ │    │ │    │ │    │ │    │ │    │ │    │                     │
│        │ Favourites           │ │    │ │    │ │    │ │    │ │    │ │    │ │    │                     │
│        │ ──────────────────── │ └▁▁──┘ └────┘ └────┘ └────┘ └────┘ └────┘ └────┘                     │
│        │ Action        2 113  │ The Matrix  Heat       Ronin   …                                     │
│        │ Comedy        1 876  │ 1999 · 8.7  1995 · 8.3                                              │
│        │ Kids 󰌾           312  │ …                                                                │
└────────┴──────────────────────┴──────────────────────────────────────────────────────────────────┘
```
Grid columns from width (min card 132 Medium / 150 Wide, × rem), rows virtualized. Key
contexts `[PosterGrid]` / `[PosterGridFiltering]`: typing filters the grid (launcher idiom,
Backspace deletes while filtering, Esc clears), Enter → detail; Shift+Enter → play/resume
fullscreen; Ctrl+S → sort & view menu (recently added, A–Z, Z–A, year, rating, custom order).
Series identical with series posters. With no VOD in any source, Movies/Series are hidden in
the sidebar and Alt+4/5 toast "Your playlists have no movies or series".

### 13.6 Details
```
Movie                                           Series
┌───────────────────────────────────────────┐   ┌──────────────────────────────────────────────┐
│ (backdrop, dimmed to bg @ 0.85)           │   │ Example Show (2020) · Drama · 7.0            │
│ ┌──────┐  The Matrix                      │   │ Plot …                                       │
│ │poster│  1999 · 2 h 16 · ★ 8.7 · Action  │   │ [Season 1] [Season 2] [Season 3]   ([ ])     │
│ │      │  A hacker learns the truth …     │   │ E01  Pilot · 42 min                     ✓    │
│ └──────┘  Cast: Keanu Reeves …            │   │ E02  Second · 44 min          ▕██████░▏      │
│ [󰐊 Resume 41:20] [Start over] [󰓎] [✓]   │   │▸E03  Third · 43 min   (opens here: next ep.) │
└───────────────────────────────────────────┘   └──────────────────────────────────────────────┘
```
Key context `[Detail]` for both pages: ↑/↓/←/→ move between buttons and episodes, `[`/`]`
switch seasons, Tab cycles sections (buttons · seasons · episodes), Enter plays/resumes,
Shift+Enter plays from the start, Ctrl+W marks watched, Ctrl+F favourite, Backspace/Esc back.
Details load lazily with a skeleton; failures keep list-level data with a toast. Resume prompt
(overlay) when progress > `resume_min_s`: "Resume from 41:20" (focused) / "Start over".
**End of playback**: an episode shows a "Next episode in 10 s" card over the video (Enter = now,
Esc = cancel → stays on the ended episode); a movie is marked watched and the player returns
to its detail page.

### 13.7 Search overlay (`/`, Ctrl+K)
```
            ┌──────────────────────────────────────────────────────────────────┐
            │ 󰍉 bbc▏                                                            │
            │ All · Live · Movies · Series · Guide                    Tab cycles│
            │ LIVE ──────────────────────────────────────────────────────────── │
            │ ▣ BBC One HD        [󰐹 My IPTV]   News at Eight · 20:00     ▕██░▏ │
            │ ▣ BBC Two           [󰒍 Free]      Gardeners' World               │
            │ MOVIES ─────────────────────────────────────────────────────────── │
            │ ▢ BBC Earth: Oceans (2019)   [󰐹 My IPTV]                          │
            │ GUIDE ──────────────────────────────────────────────────────────── │
            │ Thu 21:00  Bbc Proms · BBC Four                  󰂚 / ⟲ (past)     │
            └──────────────────────────────────────────────────────────────────┘
```
Launcher card (width 720, grows downward once typing starts), header is the query, match
highlighting (accent), source badges. Enter plays channels FULLSCREEN (no preview here; zap
collection = the channel's primary group) and opens VOD/series details; Shift+Enter plays
VOD directly. Locked items are excluded while the session is locked (`SearchItem.locked`).
No results: "No matches — Ctrl+K searches everything, Tab switches sections".

### 13.8 Player & OSD
```
┌──────────────────────────────────────────────────────────────────────────────────────────┐
│ ▣ 101  BBC One HD                                                      20:26   󰕾 80 %   │ ← top OSD
│                                                                                          │
│                                        VIDEO                                             │
│                                                                    ┌ CHANNELS (c) ─────┐ │
│                                                                    │ 100 BBC Parliament │ │
│                                                                    │▸101 BBC One        │ │
│ News at Eight                                     20:00 – 21:00    │ 102 BBC Two        │ │
│ ▕██████████████████░░░░░░░░░░░░░░░▏                34 min left     └────────────────────┘ │
│ Next  21:00 Drama                                    [HD] [5.1] [CC] [⟲]                  │
│ 󰒮  󰐊  󰒭    󰗅 English (AC-3 5.1)   󰨖 Off   󰃰 Guide   󰋚 History   󰋽 Info   󰊓              │
└──────────────────────────────────────────────────────────────────────────────────────────┘
VOD bottom: title · S02E05 · [━━━━━━━━●───────── 41:20 / 1:02:13] · −10 / +10 s · tracks
```
The OSD has two layers (UX review B1):
* **Info banner** (passive): the top and bottom bars drawn above, shown on zap, on any key and
  on mouse motion; hides after `osd_timeout_ms` (3.5 s) and never while a side panel/menu is
  open. It **does not change the key context**: ↑/↓ keep zapping, ←/→ keep recalling/seeking,
  however often they are pressed.
* **Controls focus mode**: entered only with Enter, `i`, Tab or a mouse click on a control
  (`FocusOsdControls`); pushes `PlayerOsd` (arrows move between controls / scrub, Enter
  activates); Esc leaves focus mode (`LeaveOsdControls`) and the banner times out normally.
  Unbound keys fall through (Space still pauses).
* **Esc ladder in the player**: leave controls focus → hide the banner → leave window
  fullscreen → leave the player (mini-player keeps playing). Esc never *shows* the banner.
* Type: title Display·s, body Title·s, badges Caption·s; `osd_accent` for progress/focus. Below
  700 pt the controls row collapses to play/pause · audio · subtitles · info.

Side panels: channel list `c` (current collection with now), mini guide `g` (now/next for the
group), history `h`/→ — each with key contexts `[List|ListFiltering, PlayerLive]`: ↑/↓ move the
panel cursor (no zap), Enter tunes, ←/Esc close, letters filter. Stream info `I` (codec,
resolution, fps, pixel format, HDR badge from gamma pq/hlg, audio codec/layout/rate, bitrates,
hwdec, dropped frames, cache seconds). Digits open numeric entry (`[NumericEntry, PlayerLive]`:
digits, Enter = go now, Backspace = delete digit, Esc = cancel; auto-commit after 1.5 s).
States inside the video rect: Loading (logo + name + spinner), Buffering (%), Reconnecting
(n/max + countdown), Error (glyph + copy + "r Retry · Backspace Last channel"), Radio (logo +
now playing on black), Video unavailable (no surface). Mouse: wheel = volume (fullscreen player
only — not over the Live preview), double-click toggles window fullscreen, a single click on the
Live preview opens the player. End of playback per §13.6. Esc leaves to the previous screen;
playback continues in the mini player (bottom-right 16:9, 320 Wide / 240 Medium × rem, hidden
in Compact → status-bar chip; click = back to player, ✕ = stop).

**Video-only (popped-out window, §12.3).** Same screen, same keys (zap ↑/↓, volume, pause,
mute, tracks, `c`/`g`/`h` panels) plus `PopOut` (Esc / `o` = back to the full window). The
passive banner is one bottom strip — title (⏸ when paused) and volume, then the programme on
now (live) or position / duration (VOD), Body/Caption type, no scale factor; the full OSD
returns only in controls focus mode or with a side panel open. Double-click pops back in
(instead of window fullscreen). Nothing playing: "Nothing playing · o or Esc: back to the full
window" on black.


### 13.9 Settings
```
┌────────┬──────────────────────┬─────────────────────────────────────────────────────────────┐
│sidebar │ SETTINGS             │ PLAYLISTS                                       [+ Add]     │
│        │ ▸ Playlists          │ 󰐹 My IPTV   Xtream · 9 812 ch · 48 211 movies · 3 102 series │
│        │   EPG                │   Active · expires 2027-01-01 · 1/2 connections · ts, m3u8  │
│        │   Player             │   Updated 2 h ago                   [Refresh] [Edit] [⋯]    │
│        │   Interface          │ 󰒍 Free TV   M3U URL · 11 209 ch · Failed 1 h ago: HTTP 404   │
│        │   Favourites         │ ─────────────────────────────────────────────────────────── │
│        │   Reminders          │ Edit: name · glyph · colour · URL/credentials (••••) ·      │
│        │   Parental           │ User-Agent · live format · refresh interval · server tz ·   │
│        │   Backup             │ use provider EPG · smart names · import VOD                 │
│        │   Hidden items       │                                                             │
│        │   Orphans            │                                                             │
│        │   About & logs       │                                                             │
└────────┴──────────────────────┴─────────────────────────────────────────────────────────────┘
```
Panel idiom (h/j/k/l allowed, Enter toggles, ←/→ adjust; focused text fields push
`TextInput`, which owns ←/→/Delete/Space). Reorderable rows (playlists, **EPG source priority**,
favourites lists) show a 󰇙 drag grip and move with Alt+↑/↓ or K/J. Every edit goes through
`ctx.edit_config` / the store and applies live (`PlayerCommand::ApplyConfig`). Settings list:

| Page | Setting | Key / storage | Default |
|---|---|---|---|
| Playlists | add (→ the onboarding card, `Route::Onboarding(AddSource)`) / edit / delete / reorder / refresh sources | `source` rows | — |
| | per source: name, glyph, colour, URL / file / Xtream server+user+password | `source` | — |
| | User-Agent | `SourceSettings.user_agent` | config `player.user_agent` |
| | Live format TS/M3U8/Auto | `SourceSettings.live_format` | `general.live_format` = Auto |
| | Refresh interval | `SourceSettings.refresh_h` | `general.playlist_refresh_h` = 24 |
| | Server timezone override | `SourceSettings.server_tz` | from account |
| | Use provider EPG · smart names · import VOD | `SourceSettings.*` | on |
| | Account info (read-only) | `AccountInfo` | — |
| EPG | sources (add URL/file, enable, **priority order** — grip / Alt+↑/↓, offset minutes, refresh) | `epg_source` | — |
| | Refresh interval | `general.epg_refresh_h` | 12 |
| | Keep past programmes (days) | `general.epg_keep_past_days` | 3 (≥ max catch-up) |
| | Auto name matching · fuzzy threshold | `epg.auto_name_match`, `epg.fuzzy_threshold` | on · 0.92 |
| | EPG mapping… | screen | — |
| Player | Hardware decoding | `player.hwdec` | auto |
| | Buffer seconds · instant load · demuxer max MiB · network timeout | `player.cache_secs`, `instant_load`, `demuxer_max_mib`, `network_timeout_s` | 10 · off · 150 · 15 |
| | Zapping delay | `player.zap_delay_ms` | 300 |
| | Skip small / big | `player.skip_small_s`, `skip_large_s` | 10 · 60 |
| | Audio / subtitle languages · subtitle auto · subtitle size | `player.audio_langs`, `sub_langs`, `sub_auto`, `sub_scale` | eng · eng · forced · 1.0 |
| | Reconnect attempts | `player.reconnect_max_attempts` | 8 |
| | Resume threshold · watched % | `player.resume_min_s`, `watched_pct` | 60 · 92 |
| | Default User-Agent | `player.user_agent` | VLC/3.0.21 |
| | Advanced mpv options | `player.mpv_options` | {} |
| | Pop-out size · corner (Hyprland, §21.6) | `player.popout_size`, `popout_corner` (+ `popout_last`, remembered at pop-in) | small 480 × 270 · bottom right |
| Interface | Start screen | `general.start_screen` | Auto (Live until favourites/history exist, then Home) |
| | Sidebar · density · channel numbers · logos · 24 h clock | `interface.*` | auto · comfortable · on · on · on |
| | Live preview autoplay · preview delay · guide preview · mini player · OSD timeout | `interface.*` | **off** · 1200 ms · off · on · 3500 ms |
| | UI scale (on top of Omarchy base size) · hide empty groups | `interface.ui_scale`, `hide_empty_groups` | 1.0 · on |
| | Default sort live / movies / series | `interface.*_sort` | provider / recent / recent |
| | Smart names (default) | `general.smart_names` | on |
| | Follow Omarchy theme | `integration.follow_omarchy_theme` | on |
| | Fonts: family · script fallbacks · extra files | `fonts.*` | monospace · on · [] |
| | Reminders on · lead minutes | `reminders.*` | on · 2 |
| | MPRIS · idle inhibit mode | `integration.mpris`, `idle_inhibit` | on · while playing |
| Favourites | lists: create, rename (F2), delete (Confirm), reorder (grip / Alt+↑/↓) | `fav_list` | "Favourites" (default, not deletable) |
| Reminders | pending reminders (programme, channel, time); Delete cancels | `reminder` | — |
| Parental | enable · set/change PIN · relock after · lock groups/channels (from lists via Ctrl+L) | `parental.*`, overrides | off |
| Backup | export (include passwords? history?) · import (merge/replace, restore settings?) | §19 | — |
| Hidden items | unhide groups/channels/VOD | overrides | — |
| Orphans | Enter = attach to…, Delete/x → menu with Forget | §8.3 | — |
| About & logs | version, paths, debug logging, log viewer, export log | `general.debug_log` | off |
Per channel (Ctrl+I "Edit information", `OverlayRequest::EditItem`, WP-11): rename, catch-up
offset (minutes), EPG assignment, lock, hide.

### 13.10 Onboarding
One centred card (launcher look; **no sidebar, status bar or mini-player**), steps:
1. **Welcome** (Add a playlist · Restore a backup) — choice card, key context `[Modal]`.
2. **Type** (Xtream login · M3U URL · M3U file · Try free channels; an M3U that is an Xtream
   account is upgraded automatically by the probe, with "Keep as M3U" on the Test step — §27.3,
   §27.8) — `[Modal]`.
3. **Details** (name, glyph from `SOURCE_ICONS`, colour, credentials; password masked) —
   `[TextInput, Modal]` while a field is focused. The Xtream server field is normalized with
   `xtream::urls::normalize_server` (adds `http://` when missing, strips path / query /
   trailing `/`, keeps the port); the field hint says so.
4. **Test** (`ProbeSource`: account status/expiry/connections, warnings).
   * Auth rejected / expired / banned / HTML page → the message, then back to Details with the
     offending field focused.
   * Server unreachable / timeout → "Retry" and "Add anyway".
5. **Guide** (only for M3U sources without `url-tvg`): XMLTV URL or file, or "Skip" — "You can
   add one later in Settings ▸ EPG". Skipped for Xtream and for playlists that carry `url-tvg`.
6. **Importing** (`AddSource` phases with progress per kind: channels, movies, series, guide).
   **"Open Live TV" is enabled as soon as the channels phase finishes**; movies, series and
   guide continue as jobs in the status bar.
7. **Done** (counts).

Esc goes back a step; everything keyboard-reachable. The same card is reused via
`Route::Onboarding(OnboardingStart)`: `AddSource` (Settings ▸ Playlists ▸ "+ Add", starts at
Type), `PlaylistFile(path)` (drag & drop / `tuner file.m3u`; Details with the name prefilled),
`NeedsCredentials(source)` (restored sources without password; Details for that source),
`FreeChannels` (Settings ▸ Playlists ▸ "Add free channels…"; starts at the **Presets** step that
"Try free channels" opens, §27.8 — a pick probes the preset like a typed M3U URL).

### 13.11 Help, jobs, dialogs, EPG mapping, logs
* Help (`?`, F1): generated from `keymap::help_rows`; current contexts first; two columns ≥
  760 px.
* Jobs (Ctrl+J, `[List, Modal]`): running jobs with phase + progress; Delete cancels the
  highlighted job; last 20 finished (Enter expands errors).
* Context menu (Shift+F10 / Ctrl+Enter / right-click): launcher-style, filterable, shortcuts
  right-aligned.
* Confirm (destructive = urgent button, Cancel focused), PIN (`[TextInput, Modal]`), Rename
  (`[TextInput, Modal]` — Space types a space), favourites-list picker (checklist + "New
  list…"), Edit information (Ctrl+I).
* EPG mapping (§9.4; `[EpgMapping, List]`: Enter assigns, Delete clears) and Logs (type to
  filter text; Ctrl+S view menu: minimum level, Export…) are full screens.
* **Rule: no screen reads raw keys** — every key a screen reacts to is in a keymap table, so
  the help overlay and the hint line are complete.
* *Decided in phase 3:* Help and Jobs are toggles — requesting the one on top again (`?`/F1,
  Ctrl+J, a status-bar click) closes it. Switching screens (sidebar, Alt+digits, `--view`)
  closes every open overlay; overlays belong to the screen that opened them.

### 13.12 States (copy and behaviour)

| State | Where | Presentation |
|---|---|---|
| Loading library (first launch with data) | all lists | skeleton rows (fg @ 0.04 bars), no spinner < 300 ms |
| Empty library | Home/Live | empty state 󰐹 "No playlists yet" + "Add playlist" (→ onboarding) |
| Empty group / no matches | lists | 󰍉 "No matches for “abc”" / "This group is empty" (+ "Show hidden" if items hidden) |
| No guide data | rows / grid | "No guide data" (@ 0.36); grid: one gap cell; Ctrl+E assign EPG hint |
| Guide doesn't cover the channel (WP-13) | preview, grid, guide header, Settings ▸ EPG, onboarding Done | says *why*: loading / no guide at all / guide not loaded / "Not in your guide" with the coverage ("Your guides cover 3 of 11 249 channels"); Ctrl+E = EPG mapping, or Settings ▸ EPG when there is nothing to map to. Settings ▸ EPG rows: "Matches N of M channels · guide lists X channels, Y programmes" (+ a note when under half are covered); Playlists rows "N ch (M with guide)" |
| Guide stale / ended | status bar, guide header | warning glyph "Guide updated 3 d ago · Refresh" |
| Import running | status bar, settings row | phase + % ; lists keep showing old data |
| Import failed | toast + settings row | "Couldn't refresh My IPTV: HTTP 404" (redacted), old data kept |
| Offline (network errors on jobs/images) | status bar | 󰖪 "Offline — showing saved data"; clears on next success |
| Account expired / banned / disabled | settings, onboarding test, player error | urgent "Expired on 2026-09-30"; playback 403 copy mentions the account |
| Too many connections | player error | "The provider refused the stream (account expired or too many connections)" |
| Channel offline (404) / geo-blocked (403 without account issue) | player error, row dimmed | copy per §11.4; failure cache dims row |
| DRM channel | row badge, player | "DRM-protected — can't be played" (never attempts) |
| Locked (parental) | rows, search | lock glyph, PIN prompt on open/play; hidden from search while locked |
| Reconnecting / Buffering / Loading | video rect | §13.8 |
| First guide import running | rows, grid, preview | "Loading guide… 45 %" (not "No guide data") |
| Guide day without data | grid | "No guide data for Fri 17 Oct" |
| No VOD in any source | sidebar, Alt+4/5 | Movies/Series entries hidden; Alt+4/5 toast "Your playlists have no movies or series" |
| Empty favourites list | Live groups pane, Home | "Press Ctrl+F on any channel to add it here" |
| Empty history | History group | 󰋚 "Nothing watched yet" |
| No search results | search overlay | "No matches — Ctrl+K searches everything, Tab switches sections" |
| Source disabled | lists, settings row | its groups hidden; settings row "Disabled" + Enable action |
| Source needs credentials (restored) | settings row, Live empty state | "Needs credentials" → opens the onboarding Details step |
| All items of a group hidden | lists | "Everything here is hidden" + "Show hidden" action |
| libmpv/GL failed (no `VideoSurface`) | preview, player, mini | "Video unavailable — libmpv failed to start (see Logs)"; browsing works |
| Notifications unavailable | reminders | in-app toast instead of a desktop notification (`notify` returns false) |
| Video suspended (hidden window) | — | invisible: audio continues; video resumes on the next rendered frame |

---

## 14. Search

* **Global** (`search::SearchIndex`): `nucleo::Nucleo<SearchItem>` over all visible channels,
  movies and series from the `Library` (one column "name · group · source"); rebuilt when the
  snapshot changes (injection < 100 ms for 170k items; kept by the app across overlay opens);
  `reparse(append=true)` when the query extends; `tick(10 ms)` per frame; top 200; sections All
  (interleaved by score, grouped by kind), Live, Movies, Series, Guide. Locked items excluded
  until unlocked (`SearchItem.locked`); hidden items never indexed. Search results that are
  channels play fullscreen and zap through their primary group (§13.0).
* **EPG** (`search::EpgSearch`): debounced 150 ms; FTS query (§6.3) on a runtime task with its
  own reader; stale results dropped by sequence number; results show airing/upcoming first,
  then past with ⟲ when catch-up is possible.
* **List filter** (`search::ListFilter`, implemented in the skeleton): per pane,
  `nucleo_matcher` scoring, ties keep list order; < 5 ms for 20k rows; leading spaces ignored;
  Ctrl+Backspace deletes a word. Channel haystacks are `"<number> <name>"`; an all-digit query
  ranks the exact channel-number match first. Poster grids filter the same way.

---

## 15. Keyboard

### 15.1 Model & layers
* **Contexts** (`keymap::KeyContext`): Global, List, ListFiltering, PosterGrid,
  PosterGridFiltering, Detail, Guide, PlayerLive, PlayerVod, PlayerOsd, PopOut, NumericEntry,
  EpgMapping, Search, Modal, TextInput, Panel.
* **Stack per frame**: topmost overlay's contexts, else the current screen's focused-element
  contexts (e.g. `[ListFiltering]`, `[PlayerOsd, PlayerLive]`, `[NumericEntry, Guide]`,
  `[List, PlayerLive]` for a player side panel, `[TextInput, Modal]` for a focused field), then
  `Global`. The video-only player (§12.3) adds `PopOut` right above the player's base context
  (`[PopOut, PlayerLive]`, `[List, PopOut, PlayerLive]`).
* **Resolution happens in `App::raw_input_hook`**, before egui processes input, and every event
  that resolves to an action is **removed** from the input — egui never handles it a second time
  (Enter would also click the focused button; Tab would move egui focus). Unresolved events
  flow to egui widgets (text fields). Actions are dispatched at the start of `ui`: topmost
  overlay → screen → globals; an overlay returning `OverlayOutcome::Ignored` falls back to the
  globals, so Ctrl+Q, Alt+digits, F1 and Ctrl+Shift+P keep working with an overlay open.
* **Rules** (`keymap::resolve`): first matching binding wins, innermost context first;
  printable symbols match `Event::Text` (layout-independent, shift applied), named keys and
  Ctrl/Alt chords match `Event::Key`.
  * Filterable contexts (List, ListFiltering, PosterGrid, PosterGridFiltering, Search): an
    unbound printable char becomes `FilterInput(c)` — **launcher idiom: letters type in lists
    and grids, so no j/k there**; vim keys exist only in the guide, the player and settings
    panels.
  * **TextInput owns the keyboard**: after its own table only Ctrl/Alt chords and F-keys may
    reach `Global`; Space, ←/→, Delete, Backspace… return `None` and go to the text widget;
    contexts below it are never consulted (typing a space in Rename can't confirm the dialog).
  * Modal swallows printable text it doesn't bind; PlayerOsd, PopOut, NumericEntry and
    EpgMapping fall through to the context below.
  * Key auto-repeat never produces `Parent`/`Back` (holding Backspace to clear a filter stops at
    the empty filter).
  * `Global` contains only Ctrl/Alt/F-key chords so it never steals typing.
  * **No screen reads raw keys**: every key is in a table, so help and hints are complete.
* **Esc ladder**: clear filter → close overlay/side panel → leave OSD controls focus → hide the
  info banner → leave window fullscreen → leave the player (mini player keeps playing) → back
  in the route stack. Esc never shows the banner. Alt+← and the mouse back button = Back.
  *Decided in phase 3:* Back on a lone sub-page (nothing beneath it in the route stack, e.g. a
  restored last screen) goes to its parent screen: movie / series details → Movies / Series,
  Logs and EPG mapping → Settings, the player → Live TV.
* *Decided in phase 3:* Ctrl+P (open the player) acts only while something is playing (any
  non-Idle player state); with nothing playing it does nothing rather than open an empty player.
* **Mouse parity**: everything is clickable; hover moves list cursors only after real pointer
  motion (Omarchy `pointerGate`); right-click = context menu; wheel = volume only in the
  fullscreen player; double-click on video = window fullscreen (in the video-only player: pop
  back in).
* **Discoverability**: `?`/F1 help overlay (current contexts first) and the status-bar hint line
  (`keymap::hint_line`: curated verbs, keys looked up in the tables).
* No Super bindings inside the app (Omarchy owns Super). `Ctrl+Space` is avoided (fcitx5 IME
  toggle on Omarchy). Media keys arrive via MPRIS.

### 15.2 Tables (generated: `cargo run --example keymap_md`)

#### Everywhere (`Global`)

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
| `Alt+←` | Back (also the mouse back button) |
| `F11` | Toggle window fullscreen |
| `Ctrl+P` | Open the player (from the mini player) |
| `Ctrl+Shift+P` | Play / pause |
| `Ctrl+Shift+S` | Stop playback |
| `Ctrl+Shift+O` | Pop the player out (small, on every workspace) / back in |
| `Ctrl+R` / `F5` | Refresh playlists & guide |
| `Ctrl+J` | Show background tasks |
| `F1` | Keyboard help |

#### Lists (`List`)

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

#### Lists while filtering (`ListFiltering`)

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

#### Movies & series grid (`PosterGrid`)

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

#### Movies & series grid while filtering (`PosterGridFiltering`)

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

#### Movie & series pages (`Detail`)

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

#### TV guide (`Guide`)

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

#### Player — live TV (`PlayerLive`)

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

#### Player — movies, episodes, catch-up (`PlayerVod`)

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

#### Player — on-screen controls (`PlayerOsd`)

| Keys | Action |
|---|---|
| `←` / `→` | Move between controls / scrub |
| `↑` / `↓` | Move between control rows |
| `Enter` | Activate control |
| `Tab` / `Shift+Tab` | Next / previous control |
| `Esc` | Leave the controls |

#### Pop-out player (`PopOut`)

| Keys | Action |
|---|---|
| `Esc` / `o` | Back to the full window |

#### Channel number entry (`NumericEntry`)

| Keys | Action |
|---|---|
| `0` / `1` / `2` / `3` / `4` / `5` / `6` / `7` / `8` / `9` | Type the channel number |
| `Enter` | Go now (otherwise after 1.5 s) |
| `Backspace` | Delete digit |
| `Esc` | Cancel |

#### EPG mapping (`EpgMapping`)

| Keys | Action |
|---|---|
| `Enter` | Assign the highlighted EPG channel |
| `Delete` | Clear the manual assignment |

#### Search (`Search`)

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

#### Dialogs & menus (`Modal`)

| Keys | Action |
|---|---|
| `↑` / `↓` / `←` / `→` | Move |
| `Tab` / `Shift+Tab` | Next / previous field |
| `Enter` / `Space` | Confirm |
| `Esc` | Cancel |

#### Text fields (`TextInput`)

| Keys | Action |
|---|---|
| `Tab` / `Shift+Tab` | Next / previous field |
| `Enter` | Confirm |
| `Esc` | Leave the field |

#### Settings (`Panel`)

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

---

## 16. Theme engine

### 16.1 Sources & cascade
`~/.local/state/omarchy/current/theme/colors.toml` → `Palette::parse` mirrors
`omarchy-theme-color` (legacy short names, ANSI-only themes, derived dark/darker/lighter
backgrounds via mix, bright = mix(x, white, 20 %), accent ← color4, urgent = red, mode from
`mode` → `theme_type` → `light.mode` file → luminance). Cascade, in order: theme `shell.toml` →
theme `shell.<section>.toml` files → `~/.config/omarchy/shell.toml` (user override, merged LAST
key by key onto the already merged config — the first skeleton re-parsed the theme file and
lost the section merges) → `ShellConfig` with role/reference/gradient resolution. Rounding from `hyprctl -j getoption decoration:rounding`. Missing files →
Omarchy's hard fallback palette (never a startup error). Fixtures: liftoff (dark), catppuccin-
latte (light), tokyo-night (section file), hackerman (gradient border).

### 16.2 Derived tokens (`theme::tokens::Tokens`)
`font(t) = override ?: round(base × mult)`, `space(t) = override ?: max(1, round(px × scale))`,
`scale = spacing.scale × (scale-with-font ? base/12 : 1)`; `base = [font] base-size × ui_scale`.
Every state colour is foreground at the Omarchy alpha (§13.1); list cursor = menu.selected-*;
cards = menu/popups borders; scrim = menu.scrim @ 0.5; OSD scrim fixed dark; `osd_accent` =
accent if its WCAG contrast against the scrim over mid-grey video (#2b2b2b) is ≥ 3:1, else
#f2f2f2. `Tokens::rem()` = base font / 12 (includes `ui_scale`) drives layout scaling (§12.1).

### 16.3 egui mapping (`theme::egui_style::apply`)
Applied to both egui light & dark styles (we decide, not the OS): panel fill = background,
window fill = menu background, extreme bg = darker_background, widget states = fg alphas,
selection = fg @ 0.35, hyperlinks = accent, text = foreground, corner radius = Hyprland
rounding, shadows off, item spacing control-gap × xs, button padding 10×6, interact height 28,
text styles from the type scale (heading uses the Medium family).

### 16.4 Live reload
`ThemeWatcher` (WP-01): non-recursive inotify on `~/.local/state/omarchy/current/`
(`theme.name` close-write — the last write of `omarchy-theme-set` — or `theme` moved-in),
`~/.config/omarchy/` (`shell.toml`), `~/.config/fontconfig/` (`fonts.conf` →
`FontsChanged(Arc<LoadedFonts>)`, resolved and read off-thread); 180 ms debounce; reload
off-thread with `Theme::try_load` — an error (reload raced `rm -rf current/theme` / `mv
next-theme theme`) keeps the current theme and retries after 300 ms, so the fallback palette
never flashes; ignore `next-theme`, `background`; poll `theme.name` mtime every 5 s if a watch
can't be installed. No hooks written into the user's config. Until WP-01 lands,
`ThemeWatcher::start` returns `Err` (logged; no live reload).

---

## 17. Fonts & fallbacks

* Primary: `fc-match monospace` (JetBrainsMono Nerd Font here) — text **and** icons.
  Bold: `monospace:bold`; Medium: `monospace:medium` if it resolves to another file (here it
  doesn't → Regular, so "Medium" row labels render Regular; acceptable, documented).
* Fallbacks (`fonts::resolve_script_fallbacks`, implemented): per language (ja, zh-cn, ko, ar,
  he, th, hi, bn, ta, ka, hy, am) `fc-list ':lang=xx:weight=80:slant=0' file index family`
  (weight 80 = Regular; matching "Regular" in the style list had picked
  `NotoSansCJK-Light.ttc` and `NotoSansArabicUI-Thin.ttf`), preferring Sans; `.ttc` face index
  honoured; **de-duplicated by file path** (one CJK collection covers ja/zh/ko: ~19 MB once,
  not three times). Verified here: `NotoSansCJK-Regular.ttc`, `NotoSansArabic-Regular.ttf`.
* Threading: `FontSet::resolve` (~145 ms of subprocesses for 12 languages) and `FontSet::load`
  (file reads; bytes cached per (path, index) for the process) run off the UI thread — at
  startup on a helper thread joined in the creation callback, on reload in the theme watcher;
  the UI only calls `LoadedFonts::install`.
* egui families: Proportional = Monospace = [primary, fallbacks…, egui defaults];
  `Name("medium")`, `Name("bold")`.
* Shaping: epaint has no shaping/bidi. `fonts::display_text` (implemented) reshapes Arabic into
  presentation forms (`ar-reshaper`) and reorders RTL runs visually (`unicode-bidi`) for
  single-line labels; ASCII fast path. Indic conjuncts render unshaped (limitation; revisit if
  egui gains shaping).

---

## 18. Images

```
get(url, kind) ─ hit ─▶ TextureHandle (LRU touch)
     │ miss → "wanted" map (url → last frame requested)
     ▼
tokio task: wanted within 2 s? ─no─▶ drop
     │ yes
disk cache ~/.cache/tuner/img/<hh>/<blake3>.bin ─hit─▶ decode
     │ miss: HTTP (≤ 6 concurrent, ≤ 2 per host, 8 MiB cap, source User-Agent) → write .bin
decode on blocking pool (≤ 2): image (png/jpeg/webp/gif first frame) / resvg (svg) →
     fast_image_resize into the kind's box (logo 128², poster 342×513, backdrop 1280×720,
     still 400×225) → ColorImage
     ▼
UI pump: ≤ 4 MiB of texture uploads per frame (one backdrop ≈ 3.7 MB) → LRU over texture
bytes (192 MiB budget, evict entries unused ≥ 2 frames first); wanted-map entries older than
2 s pruned on every pump
```
Untrusted input limits: raster decode through `image::ImageReader` with `Limits {
max_image_width: 8192, max_image_height: 8192, max_alloc: 64 MiB }`; SVG sources > 1 MiB are
rejected, parsed with an EMPTY `usvg` fontdb (never `load_system_fonts`) and rasterized at most
at the kind's box. Fetching goes through a private `Fetch` trait (HTTP impl + test fake).
Until WP-09 lands `get` returns `Failed` (monogram) and `pump` is a no-op.
Failures negative-cached (memory 1 h, `.fail` marker 24 h); transient network failures
(timeouts, 5xx, offline, TLS) memory only, backing off per image 2 → 8 → 32 → 60 min (WP-13: a
blip must not hide logos for an hour; an unreachable host is retried a few times an hour). The
texture budget is a hard cap: an image that doesn't fit beside the visible set waits (loading)
until something scrolls away. SVG: ≤ 10 000 nodes, render > 3 s = invalid. "Source User-Agent"
= by host (WP-13 decision): `get(url, kind)` has no source, so images on the host a playlist is
served from use that playlist's User-Agent, others the default. Logos never show spinners (neutral
placeholder, then monogram on failure); posters show a skeleton. Disk cache pruned to 500 MB
by maintenance.

---

## 19. Backup / export

`store::backup` JSON (`format: "tuner-backup", version: 1`): sources (credentials only if "Include
passwords" is ticked; default off), EPG sources, favourites lists + items, item/group overrides,
watch progress, history (optional), reminders, manual EPG mappings (by EPG source *name*),
raw `config.toml`. Written 0600 via the portal file picker. Restore: Merge (match sources by
kind + name + URL/server) or Replace; then every restored source is refreshed and user data
re-attaches through fingerprints (§8.3). Sources restored without passwords are listed as
"needs credentials". The backup's settings are applied only when "Restore settings" is on (it
follows the mode: on for Replace, off for Merge, so a Merge that brings back favourites keeps
the current settings; the onboarding restore always applies them). Machine-specific settings
(fonts, hwdec, mpv options, UI scale, debug log) stay, and an existing PIN is never replaced.

---

## 20. Security & redaction

* **At rest**: DB 0600 inside a 0700 data dir (created before SQLite opens it; WAL/SHM inherit
  the mode); `~/.cache/tuner/{playlists,epg}` are 0700 and downloads are created 0600 (Xtream
  `get.php` playlists and `xmltv.php` guides carry user/password in every URL); downloaded
  playlists are deleted after a successful import; config contains no secrets; backup without
  passwords by default; IPC socket 0600 in `$XDG_RUNTIME_DIR/tuner` (0700).
* **In memory/types**: `Secret<T>` for passwords, credential-bearing URLs (M3U URL, xmltv.php,
  stream URLs) — `Debug` prints `Secret(***)`.
* **Redaction** (`redact::redact`, implemented + tests): registered secrets (all usernames/
  passwords/URL tokens loaded at startup and on source edits, plus the Xtream logins an M3U
  import finds in its stream URLs — a pair counts once two entries with different stream ids
  carry it, so a CDN path shaped like one never becomes a secret; registered as found and kept in `meta`
  (`playlist-logins:<id>`) for the next start, so a playlist that stays M3U has its prefixed
  short-form URLs `host/<prefix>/<u>/<p>/<id>` masked too), URL userinfo, credential query
  parameters (`username password pass token auth key sig…`), Xtream path segments
  (`/live|movie|series|timeshift/<u>/<p>/`, and the bare `host/<u>/<p>/<id>`). Applied to: every log line (tracing layer), every
  `Error` Display, mpv log messages, job errors, toasts, stream-info panel, Settings URL
  display. The secret registry is process-global: tests never call `redact::clear_secrets`
  (parallel tests race) and use a unique secret per test instead.
* **Never on a command line**: mpv is embedded (no `ps` exposure); `force-media-title` keeps
  URLs out of mpv's title; MPRIS exposes titles and logo URLs only (http(s) logos are fine).

### 20.3 Parental PIN
4–8 digits, stored as hex(blake3(salt ‖ pin)) with a random salt in config; locked groups/
channels/VOD require the PIN to open/play; unlock lasts until restart or `relock_min`; three
wrong attempts → 30 s cooldown; locked items hidden from search while locked. It's a speed bump
for children, not security (documented in Settings).

---

## 21. Desktop integration

### 21.1 MPRIS (`platform::mpris`)
`org.mpris.MediaPlayer2.tuner` via `mpris-server` (tokio) — implement `RootInterface +
PlayerInterface` on a `Send` struct served by `mpris_server::Server<T>` (`mpris_server::Player`
wraps a non-`Send` `LocalServer` and can't be spawned on the runtime): Identity "Tuner", DesktopEntry
"tuner", CanRaise/CanQuit; PlaybackStatus from `PlayerState` (active states → Playing, Paused,
else Stopped); Metadata: trackid, title (programme if live, else item), artist [channel],
artUrl (logo/poster), length (VOD); CanGoNext/Previous = live with a zap list; CanSeek =
seekable. Commands go straight to the `PlayerHandle` (work while hidden). Omarchy media keys
(`omarchy-shell media playPause|next|previous`) and the bar's media widget then work.

### 21.2 Idle inhibit — how Omarchy 4 decides (investigated)
`/usr/share/omarchy/shell/plugins/services/idle/Service.qml` uses Quickshell `IdleMonitor`
(ext-idle-notify-v1) with **`respectInhibitors: true`** and a user "stay awake" state file
(`~/.local/state/omarchy/indicators/stay-awake`). So the compositor's Wayland idle inhibitors
count: `zwp_idle_inhibit_manager_v1` inhibitors on visible surfaces and Hyprland `idle_inhibit`
window rules (Omarchy ships `tag noidle → idle_inhibit always` and `fullscreen` rules for games).
There is **no `org.freedesktop.ScreenSaver`** on the session bus and xdg-desktop-portal-hyprland
does not implement `Inhibit`, so D-Bus inhibition would be ignored; `systemd-inhibit` does not
affect ext-idle-notify either.
**Mechanism**: `platform::idle::IdleInhibitor` creates a `zwp_idle_inhibitor_v1` on Tuner's own
`wl_surface` through a wayland-client connection built from eframe's foreign `wl_display`
(`Backend::from_foreign_display`, own event queue) — held while `PlayerState::is_active()` and
the mode allows (`WhilePlaying` default / `Fullscreen` / `Never`), dropped on pause/stop/error.
The compositor honours it only while the surface is visible — correct for video. Fallback for
odd setups: the commented `idle_inhibit = "fullscreen"` rule in `packaging/hypr/tuner.lua`. We
never touch the user's stay-awake file.

### 21.3 Notifications & reminders
`notify-rust` (zbus/tokio) → Omarchy shell notification server (accent border). Reminder
scheduler (tokio) sleeps until `start − lead`, sends "‹programme› starts in 2 min on
‹channel›" with action **Watch** (tune + raise via `hypr::focus_self`); missed reminders that
already ended are dismissed silently, ones still airing fire with "started N min ago".
`notify` returns false when no notification server answers → in-app toast instead. The app
calls `ReminderScheduler::refresh` on `DataScope::Reminders`; `PlatformEvent::ReminderWatch`
carries the channel's `ItemRef` (resolved by the app through `LiveIndex::by_ref`).
Reminders fire only while Tuner runs (v1).

### 21.4 Single instance & CLI
`$XDG_RUNTIME_DIR/tuner/tuner.sock`; a second `tuner [--view guide] [--play …] [file/url]`
hands its request over and exits 0 (stale socket removed). Works with `omarchy-launch-or-focus
'^tuner$'` (anchored: the pattern also matches window titles) and the omarchy-menu entries. Request handling (`TunerApp::handle_ipc`, also for the
first instance's own CLI): `--view` switches route; `--play X` and positional URLs / media files
play immediately (`MediaRef::Url`, fullscreen player); positional `.m3u`/`.m3u8` files open the
onboarding card at Details (`Route::Onboarding(PlaylistFile)`).
*Decided in phase 2:* inputs are made absolute by the invoking process before the handoff (the
running instance has another working directory); a local `file://` URI (desktop `%U`, file
managers, `gio open`) is decoded to its path and treated like any other path, while
`file://host/…` stays a URL. `tuner --refresh` never hands off: with an instance running it
does nothing (§9.5); while it runs it holds the instance lock itself.

### 21.5 Files & windows
File pickers through the XDG portal (`rfd` xdg-portal → portal-gtk, auto-floated, follows
prefer-dark/light). `hypr::focus_self` uses Hyprland 0.56's Lua dispatcher
(`hl.dsp.focus({ window = "class:^(tuner)$" })`) with the legacy `focuswindow` fallback, like
`omarchy-launch-or-focus`.

### 21.6 Pop-out player (`platform/popout.rs`, `ui/popout.rs`)
A floating **pinned** Hyprland window is shown on every workspace — Omarchy's Super+O
(`omarchy-hyprland-window-pop`: float, 1300 × 900, centre, pin, raise, tag `pop`) does that
for any window. Tuner does it for itself with `o` in the player or Ctrl+Shift+O anywhere:
* **IPC**: requests on `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock` (what
  `hyprctl` writes: `j/clients`, `j/monitors`, `/dispatch …`), from the `hypr-popout` worker
  thread; the UI only sends `PopRequest`s and gets `PlatformEvent::PopOut` back. Our window is
  the mapped client with **our pid** (never matched by class); its address must be `0x`+hex
  before it is put into a dispatcher.
* **Syntax**: Lua first (`hl.dsp.window.float({ window = "address:0x…", action = "enable" })`,
  `pin`, `resize { x, y }`, `move { x, y }`, `alter_zorder { mode = "top" }`, `tag { tag =
  "+pop" }` — Hyprland 0.56 parses `action = enable|disable|toggle`), classic on a non-`ok`
  reply (`setfloating`/`settiled`, `pin` (toggle — sent only when the state differs),
  `resizewindowpixel exact W H,address:…`, `movewindowpixel exact X Y,address:…`,
  `alterzorder top,…`, `tagwindow ±pop …`), exactly like `omarchy-hyprland-window-pop`. The
  syntax that worked first is kept for the session.
* **Pop-out**: (unless already) float → size → move → (unless already) pin → raise → tag `pop`
  (Omarchy rounds `tag:pop` windows). Size: Settings ▸ Player ▸ Pop-out size, Small 480 × 270
  (default) or Medium 640 × 360; place: the chosen corner (default bottom-right) of the
  window's monitor, inside its reserved area (bar), 16 px margin. A fullscreen window is taken
  out of fullscreen first (the worker retries for 1 s until Hyprland sees it).
* **Pop-in** undoes only what pop-out changed: unpin (unless it was pinned), untag (unless it
  was tagged), then back to its floating geometry if it was floating, else tiled — on the
  workspace the user is on now (a pinned window travels). The pop-out's place and size at
  that moment are remembered (`player.popout_last`, relative to the monitor, clamped on
  reuse); changing size or corner in Settings forgets it. Toggling while popped out by
  Super+O (no record) does what Super+O's second press does: unpin, untag, tile.
* **State**: the `hypr-events` thread reads `.socket2.sock` and reports our window's
  floating / pinned flags (`changefloatingmode>>ADDR,0|1`, `pin>>ADDR,0|1`; `openwindow`
  re-looks our window up, `closewindow` forgets it). They drive the video-only layout
  (§12.3) and end our pop-out when someone else unpins it (Super+O pressed again).
* **Outside Hyprland** (no `HYPRLAND_INSTANCE_SIGNATURE`) the key only toasts that the
  pop-out needs Hyprland. Failures (refused dispatch, window not listed) are toasted and leave
  the layout docked. Unit tests never talk to a compositor: `HyprIpc` has a fake that
  answers like Hyprland in either syntax, and the app takes a recording `PopOutBackend`.
* A popped-out window is always visible, so its playback never meets the hidden-workspace
  limits of §11.1.

---

## 22. Packaging & licence

* **Licence**: GPL-3.0-or-later (Arch's `mpv`/libmpv is built with GPL components, making the
  combined work GPL; MPL-2.0 deps (nucleo, mpris-server) are compatible).
* `packaging/tuner.desktop` (MimeType M3U handlers, actions Guide/Live, `StartupWMClass=tuner`),
  `packaging/icons/tuner.svg` (square-cornered; PKGBUILD installs to hicolor/scalable — PNG
  sizes can be rendered with `rsvg-convert` if a theme needs them), `packaging/hypr/tuner.lua`
  (`-default-opacity` tag + `opacity "1 1"`, **`render_unfocused = true`** so playback keeps
  rendering on other workspaces (§11.1), optional idle fallback and Super+Shift+T binding),
  `packaging/omarchy/omarchy-menu.jsonc` (TV submenu), `packaging/PKGBUILD` (x86_64 + aarch64,
  depends mpv/xz/fontconfig/JetBrains Mono Nerd/wayland/mesa; builds the local checkout into
  `packaging/src/target`, installs the snippets under `/usr/share/tuner/` and the docs under
  `/usr/share/doc/tuner/`; the licence is the common SPDX text, so no licence file is installed).
* User install without a package: `cargo install --path .`, copy the desktop file to
  `~/.local/share/applications/`, icon to `~/.local/share/icons/hicolor/scalable/apps/`, merge the
  Hyprland and menu snippets. The Hyprland snippet is a module: `~/.config/hypr/tuner.lua` +
  `require("hypr.tuner")` (Omarchy's bootstrap puts `~/.config/?.lua` on `package.path`).
* User-facing docs: `README.md` (install, sources, guide coverage, keys — the table between the
  `keymap` markers is regenerated with `cargo run --example keymap_md`), the manual test
  checklist `docs/MANUAL_CHECKLIST.md` (§23).

---

## 23. Testing strategy

| Layer | How | Where |
|---|---|---|
| Pure logic | unit tests next to code: keymap resolution (banner vs controls, TextInput ownership, repeat, consumption-relevant rules), reconnect policy, player state machine (scripted inputs + injected time, entry tracking, HTTP classification, resolve failures), tokens/contrast, palette cascade, redaction, identity keys (golden), URL builders (golden), catch-up expansion, xmltv time parsing, library indexes, NowNextCache, ListFilter, PIN session | `src/**` `#[cfg(test)]` |
| Parsers | fixture files + `insta` snapshots; edge-case M3U (BOM, CRLF, quoting, pipe headers, DRM, catch-up attrs), XMLTV (entities, multi-lang, tz forms, missing stop, unknown channel), Xtream JSON quirks (numbers-as-strings, `info: []`, episodes object/array/empty, base64 EPG, HTML 200) | `tests/{m3u,xmltv,xtream}.rs`, `tests/fixtures/**` |
| Store | `Store::open_temp` (real WAL file; never shared-cache in-memory) + `tests/support/seed.rs` raw-SQL seeders: upsert/sweep, reattach scenarios (incl. progress, reminders, manual EPG), generation flip atomicity with a concurrent reader, FTS consistency across 3 re-imports + prune (no orphan FTS rows), query plans (`EXPLAIN QUERY PLAN` uses `programme_grid`), backup round-trip | `tests/store_*.rs` |
| Mock Xtream | axum server in-process (`tests/support/mock_xtream.rs`, WP-04 milestone A) with auth/expired/403/HTML/delay switches, `xmltv.php`, `get.php`, stream paths redirecting to local media; full add-source → library e2e | `tests/xtream.rs`, `tests/import_e2e.rs` |
| Player (headless) | `Player::start` with `cfg.player.mpv_options = {vo: "null", ao: "null"}`, fake `MediaResolver` + recording `PlayerStore`; ffmpeg-generated media (`scripts/gen-test-media.sh`, `tests/support/media.rs` WP-07 milestone A), local HTTP server with fault injection (drop after N bytes, segment 404s) → state transitions, reconnect, per-file options, stop-before-start ordering, progress saving; skipped with a note if ffmpeg is missing | `tests/player_mpv.rs` |
| UI | `tuner::ui::testing::TestUi` (real services in a temp profile, headless mpv, owns every `UiCtx` part) + `egui_kittest` harnesses built by the test; fake `EpgReader` for the guide; key dispatch layers, Esc ladder, list navigation & filter, guide focus model, help overlay rows | `tests/{keymap,widgets,guide,onboarding,parental}.rs` |
| Smoke (skeleton) | store opens private/migrated, caches 0700, seeders consistent, every screen and overlay handles actions and draws without panicking | `tests/smoke.rs` |
| Benchmarks | criterion targets declared in `Cargo.toml`, one owner each (§24.2), on `scripts/gen-big-fixtures.py` output (100k M3U, 415k-programme XMLTV) and seeded DBs (5 000 channels) | `benches/{m3u,xmltv,store,search,import,guide}.rs` |
| Manual checklist | theme switch live (dark ↔ light, `white` theme OSD), font change, tiled/half/full layouts at text size 12 and 16, media keys, idle (lower screensaver timeout to 30 s), other-workspace playback 60 s with and without `render_unfocused`, 1 h soak, fullscreen toggle, real providers (iptv-org, Pluto via i.mjh.nz EPG) | WP-13: `docs/MANUAL_CHECKLIST.md` |

`cargo test` must stay green without network, ffmpeg or a Wayland session (those tests skip).
Lint gate: `cargo clippy --all-targets -- -D warnings` (baseline is zero; stub modules carry a
scoped allow in `lib.rs` that each WP removes for its module). Tests never call
`redact::clear_secrets`.

---

## 24. Implementation plan

### 24.1 Rules
* **Phase 0 (WP-00, architect) hardens the skeleton before any worktree exists** (§24.3). It
  ends with `git add -A && git commit` on `main` (including `Cargo.lock`) and the tag
  `skeleton-v1`. Every WP branches from that tag in its own git worktree.
* **Frozen = architect-owned.** Frozen files (single authoritative list; `lib.rs` points here):
  `Cargo.toml`, `Cargo.lock`, `.gitignore`, `src/lib.rs`, `src/model/**`, `src/msg.rs`,
  `src/error.rs`, `src/config.rs`, `src/paths.rs`, `src/cli.rs`, `src/store/mod.rs`,
  `src/store/migrations.rs` (WP-02 may APPEND `MIGRATIONS` entries with sign-off),
  `src/store/migrations/0001_init.sql`, `src/sources/mod.rs`, `src/sources/identity.rs`,
  `src/sources/xtream/mod.rs`, `src/sources/xtream/urls.rs`, `src/ui/mod.rs`, `src/ui/data.rs`,
  `src/ui/testing.rs`, `src/ui/screens/mod.rs`, `src/ui/overlays/mod.rs` (incl. `build`),
  `src/ui/parental.rs`, `tests/support/mod.rs`, `tests/support/seed.rs`, `tests/smoke.rs`,
  `scripts/gen-big-fixtures.py`.
* **Frozen items inside WP-owned files** (bodies implemented in Phase 0; the owning WP may not
  change them without sign-off): `store::library::{LiveIndex, VodIndex, FavIndex}` accessors and
  `from_parts`; `store::sources::{register_secrets, account_limits}`;
  `sources::http::sniff_compression`; `sources::catchup::{parse_kind, in_window}`;
  `search::ListFilter`; the skeleton bodies of `player/{render,events,controller,state}.rs`
  (WP-07 may harden them, keeping the invariants of §11 and the spike).
* Any change to a frozen file or item creates tag `skeleton-v1.N`; every WP rebases onto it.
* **Pub signatures are frozen**; private fields/items and function bodies belong to the file's
  WP. Need a new cross-module type or function? Propose it to the architect; it lands in the
  frozen file first, then everyone rebases.
* **`todo!()` is allowed only in functions that no other WP's code or tests reach before the
  owner has merged** (i.e. outside the "Hard deps" column of §24.2: WP-11/12 call widgets only
  after WP-10a merged; real-data UI tests need WP-02). Anything reachable earlier (every-frame
  UI calls, shared primitives, integration start functions) has a documented SKELETON SAFE
  DEFAULT (e.g. `ImageCache::get → Failed`,
  `JobManager::submit → Finished(Err)`, `VideoHost::mini_player → None`, integration `start →
  Err`, overlay placeholders that close on Esc). `grep -rn 'todo!' src` must show only
  WP-internal bodies.
* Markers: `todo!("WP-xx")` = body to implement; `SKELETON SAFE DEFAULT` = working placeholder
  the owner replaces; `SKELETON-STUB(WP-xx)` = remaining wiring placeholder (only `--refresh`).
* Each WP adds only files it owns (tests named after its area), keeps the merge gate green, and
  removes the `#[allow(dead_code, unused_*)]` of its module in `lib.rs` once its bodies land
  (sign-off is implicit for that one-line change).
* Fixtures are append-only for everyone; only the owner of a fixture directory edits existing
  files. Docs (`docs/**`, `README.md`) are owned by the architect / WP-13; WPs attach doc diffs
  to their merge request (WP-10a's `keymap_md` output for §15.2, etc.).
* **Merge gate** (every merge): `cargo build && cargo test && cargo clippy --all-targets -- -D
  warnings` + launch-and-navigate smoke (launch, sidebar/Alt+digits, theme switch, help
  overlay). From the merge of WP-05b + WP-11 on, also "play a local file"
  (`tuner target/test-media/testsrc.mp4`).

### 24.2 Work packages (authoritative; file ownership is disjoint)

| WP | Title | Files owned | Hard deps (for green tests) | Phase |
|---|---|---|---|---|
| WP-00 | Skeleton hardening (architect) | all frozen files and items of §24.1 | — | 0 |
| WP-01 | Theme engine & fonts | `src/theme/**`, `src/fonts.rs`, `tests/theme.rs`, `tests/fixtures/theme/**` | — | 1 |
| WP-02 | Store: repositories, imports, library load, backup | `src/store/{sources,live,vod,epg,user,import,library,backup,sql}.rs` (minus frozen items), new `000N_*.sql` migrations, `tests/store_*.rs`, `benches/store.rs` | — (seed + fixtures) | 1 |
| WP-03 | M3U & smart names | `src/sources/{m3u,smart_names}.rs`, `tests/{m3u,identity}.rs` (identity = golden/property tests of frozen bodies), `tests/fixtures/m3u/**`, `benches/m3u.rs` | — | 1 |
| WP-04 | HTTP client & Xtream | `src/sources/http.rs` (minus `sniff_compression`), `src/sources/xtream/{client,types}.rs`, `tests/support/mock_xtream.rs` (**milestone A, day 2**), `tests/xtream.rs`, `tests/fixtures/xtream/**` | — | 1 |
| WP-05a | XMLTV, matching, catch-up | `src/sources/{xmltv,epg_match,catchup}.rs` (minus frozen items), `tests/{xmltv,epg_match,catchup}.rs`, `tests/fixtures/xmltv/**`, `benches/xmltv.rs` | — | 1 |
| WP-07 | Player core hardening | `src/player/**`, `tests/player_mpv.rs`, `tests/support/media.rs` (**milestone A, day 2**), `scripts/gen-test-media.sh` | — (fake `MediaResolver` / `PlayerStore`) | 1 |
| WP-08 | Platform, security, packaging | `src/platform/**`, `src/ipc.rs`, `src/redact.rs`, `src/logging.rs`, `packaging/**`, `tests/{ipc,redact}.rs` | reminders e2e: WP-02 | 1 |
| WP-09 | Images & search | `src/images.rs`, `src/search.rs` (minus `ListFilter`), `tests/{images,search}.rs`, `tests/fixtures/images/**`, `benches/search.rs` | EPG search e2e: WP-02; network path: WP-04 (unit tests use the private `Fetch` seam) | 1 |
| WP-10a | Keymap, widgets, generic overlays | `src/keymap.rs`, `src/ui/layout.rs`, `src/ui/widgets/**` (minus `poster.rs`), `src/ui/overlays/{help,jobs,dialogs}.rs`, `examples/keymap_md.rs`, `tests/{keymap,widgets}.rs` | — | 1 (merges first among UI work) |
| WP-05b | Resolve | `src/sources/resolve.rs`, `tests/resolve.rs` | WP-02 (seed-only tests may start in phase 1) | 2 |
| WP-06 | Jobs & import pipelines | `src/jobs/**`, `benches/import.rs`, `tests/import_e2e.rs` | WP-02, WP-03, WP-04 (mock), WP-05a | 2 |
| WP-10b | App shell & wiring | `src/{app,services,main}.rs` | WP-01, WP-06, WP-08, WP-09 for full behaviour (safe defaults keep it runnable earlier) | 2 |
| WP-11 | Live TV, guide grid, player UI | `src/ui/{epg_grid,video_host}.rs`, `src/ui/screens/{live,guide,player}.rs`, `src/ui/overlays/{programme,tracks,edit_item}.rs`, `tests/guide.rs`, `benches/guide.rs` | WP-10a; real data WP-02 (tests use seed + fake `EpgReader`); playback smoke WP-05b, WP-07 | 3 |
| WP-12 | Home, VOD, settings, onboarding, search UI, PIN UI | `src/ui/screens/{home,movies,series,movie_detail,series_detail,settings,onboarding,epg_mapping,logs}.rs`, `src/ui/widgets/poster.rs`, `src/ui/overlays/{search,resume}.rs`, `tests/{onboarding,parental}.rs` | WP-10a, WP-02, WP-09; onboarding e2e: WP-04, WP-06, WP-10b; mapping: WP-05a; pickers & log export: WP-08 | 3 |
| WP-13 | Integration, performance & release | `docs/**`, `README.md`, `tests/e2e_*.rs`, removal of leftover `lib.rs` allows, the manual checklist | all | 4 |

Dependency graph (→ = "must be merged before"):
```
WP-00 ─▶ { WP-01, WP-02, WP-03, WP-04(milestone A first), WP-05a, WP-07(milestone A first), WP-08, WP-09, WP-10a }
WP-02 ─▶ WP-05b ;  { WP-02, WP-03, WP-04, WP-05a } ─▶ WP-06
{ WP-06, WP-08, WP-01, WP-09 } ─▶ WP-10b
WP-10a ─▶ { WP-11, WP-12 }  (WP-11/12 may start real UI work once WP-10a merged)
{ WP-11, WP-12, WP-10b, WP-05b } ─▶ WP-13
```

#### Acceptance criteria & tests per WP
Tags: **[unit]** = runs in the WP's own tests; **[e2e after X]** = turns green once X merged;
**[bench]** = named bench + dataset; **[manual]** = checklist item verified by WP-13 unless the
WP can do it itself.

**WP-00 Skeleton hardening** — [unit] `cargo test` + `clippy -D warnings` green; no `todo!`
reachable from another WP (`smoke::test_ui_runs_every_screen_and_overlay` draws every screen and
overlay); seeders produce a consistent DB; migrations tests (FTS delete by rowid, no id reuse,
FK discipline, `fp` columns); identity and URL golden tests; then commit + tag `skeleton-v1`.

**WP-01 Theme & fonts** — [unit] all fixture themes parse; the cascade matches
`omarchy-theme-color` for ANSI-only themes (hackerman) and light detection (latte);
`shell.toml` references, gradients, section files, then the user override merged last key by
key; Liftoff tokens equal omarchy.md §2 tables (font 10/11/12/13/14/16/24/28; control-height 28;
panel-padding 18); `osd_accent` falls back on low-contrast accents; a temp-dir test reproduces
`omarchy-theme-set`'s rm/mv/write sequence and gets exactly one `Changed` within 500 ms and
never a fallback palette; [manual] switching themes recolours a running app; fonts resolve
JetBrains Nerd + Regular-weight CJK/Arabic/Hebrew fallbacks, each file once; `display_text`
reorders an Arabic+Latin label; [unit] startup font work runs off the UI thread.

**WP-02 Store** — [unit] every `todo!` implemented; re-import of identical data keeps row ids;
no `INSERT OR REPLACE` on FK-referenced tables (grep test); vanished items swept only for
completed kinds; reattach scenarios of §8.3 incl. ambiguous → orphan and progress / reminder /
manual-EPG reattach; fingerprints computed inside writes; EPG generation flip atomic for a
concurrent reader (temp WAL DB); FTS has no orphan rows after 3 re-imports + prune + EPG
source deletion; `delete_source` leaves no orphans in any table; window query uses
`programme_grid` (`EXPLAIN QUERY PLAN`); `now_next` applies tvg-shift; backup export → wipe →
restore round-trip equality; [bench `benches/store.rs`, seeded DB] 100k-channel import (2 000-row
batches) < 3 s release, window query < 1 ms, `library::load` 100k < 400 ms.

**WP-03 M3U & smart names** — [unit] `edge_cases.m3u` snapshot exact (titles with commas,
single/no-quote attrs, `;` groups, `#EXTGRP`, VLC opts, pipe headers, DRM flag, relative URL
resolution, bare URL entry, header `url-tvg` list, catch-up precedence via
`catchup::parse_kind`); BOM/CRLF/CR-only; `html_error.m3u` → `NotM3u(Html)`;
`hls_media.m3u8` → `HlsMediaPlaylist`; `smart_names.tsv` passes; identity golden + property
tests (`normalize_name` keeps `+1`, keys stable); `detect_xtream_url` for get.php/player_api;
[bench `benches/m3u.rs`, `big.m3u`] 100k entries parse < 1.5 s release.

**WP-04 HTTP & Xtream** — milestone A: `tests/support/mock_xtream.rs` merged by day 2. [unit]
every `tests/fixtures/xtream` file deserializes; auth outcomes (ok/rejected/expired/403/HTML-200/
empty `[]`) map to the right `Error`/`AccountInfo`; base64 EPG decided per response; downloads
report progress, cancel within 200 ms, honour 304, create `.part` with mode 0600, sniff gzip/xz
by magic bytes; no error Display ever contains the test password (scan, unique secret per test).

**WP-05a XMLTV, matching, catch-up** — [unit] `basic.xml(.gz)` parsed exactly (entities across
`GeneralRef` events, preferred-language title, `+0100`/`Z`/`BST`/no-zone, missing stop, unknown
channel, unparsable start skipped and counted); `parse_time` table; `xmltv_ns` and onscreen
episode numbers; matcher steps 1–5 incl. ambiguity rule; `suggest` with a prefilter (no full
scan); catch-up table tests for every kind (Xtream server tz across a DST boundary, granularity,
correction + tvg-shift (displayed→EPG) + user offset, `{offset:X}`, Flussonic rewrites, capping
at now); [bench `benches/xmltv.rs`, `big.xml.gz`] 415k programmes parse < 1 s.

**WP-05b Resolve** — [unit with seed] Xtream live (format pick), movie, episode, catch-up, M3U
headers merge, ad-hoc URL/file (`MediaRef::Url`), DRM → `BadPayload("drm")`, resume threshold;
[e2e after WP-02] against real store reads.

**WP-06 Jobs & pipelines** — [unit] coalescing only while Queued/Connecting + rerun flag,
cancellation (no sweep), throttled progress, follow-up submits, scheduler staleness policy and
`player_busy` skip; [e2e after WP-02/03/04/05a] with the mock panel: add Xtream source → library
has channels/movies/series, provider EPG imported and mapped, account info stored and reported in
`SourceRefreshed`; refresh with renumbered stream ids keeps favourites (reattach); M3U file
import; failing refresh keeps the old catalogue and records a redacted error; [bench
`benches/import.rs`] wired (gates run by WP-13). Job failures are logged at WARN or above under
target `tuner::jobs` (the Jobs overlay's Problems list reads them from the log buffer; there is no
job-history API — decided after phase 1).

**WP-07 Player core** — milestone A: `tests/support/media.rs` + `scripts/gen-test-media.sh` by
day 2. [unit] state machine covers every transition of §11.3 (entry tracking incl. the fast
switch sequence, zap coalescing, load timeout, stall, buffering timeout, VOD resume after
reconnect, stable reset, HTTP classification only while Loading, resolve failure keeps the
current stream); [unit, headless libmpv] VOD EOF → Ended, live HLS server killed → Reconnecting
→ recovers, 403/404 classified via the fault server, per-file User-Agent/Referer with commas
reach the server, progress saved every 10 s and on stop (recording `PlayerStore`),
stop-before-start waits for END_FILE with a limit of 1, `ApplyConfig` changes cache/zap delay;
[manual] other workspace 60 s with and without `render_unfocused` (audio continuity,
`frame-drop-count`, CPU, video resumes), fullscreen toggle, frame-pacing numbers for
25/30/50/60 fps; `report_swap`/`FLIP_Y`/teardown invariants untouched.

**WP-08 Platform & security** — [unit] redaction (URLs, headers, registered secrets, mpv lines;
unique secrets per test); log file rotation at 5 MB; IPC handoff (view switch, exit 0, stale
socket); [e2e after WP-02] reminder fires and "Watch" resolves by `ItemRef`; [manual]
`busctl --user list` shows `org.mpris.MediaPlayer2.tuner`; `playerctl -p tuner
play-pause|next|metadata` work while hidden; Omarchy media keys; with the screensaver timeout
lowered to 30 s, playback prevents it and pause re-enables it; `makepkg` builds;
`desktop-file-validate` passes; the Hyprland snippet gives opacity 1 and `render_unfocused`.

**WP-09 Images & search** — [unit] png/jpeg/webp/gif/svg fixtures decode into the kind's box;
oversized images (20k×20k header) rejected by limits; SVG > 1 MiB rejected; disk hit path does
no network (fake `Fetch`); ≤ 6 concurrent fetches; stale requests skipped; ≤ 4 MiB uploads per
frame; LRU budget enforced with a tiny budget; negative cache; global search sections + append
reparse + locked exclusion; [e2e after WP-02] EPG search debounce + stale-result drop; [bench
`benches/search.rs`] 170k items tick < 16 ms.

**WP-10a Keymap, widgets, overlays** — [unit] keymap tests (layering, TextInput ownership,
repeat, banner vs controls, every context has help rows), widget kittests (list nav/filter/hover
gate, row states, toggle, seek bar, toast stacking above an avoid rect), help overlay complete;
§15.2 regenerated with `keymap_md`; top glyph strip below 480·rem.

**WP-10b App shell** — [unit/kittest with `TestUi`] key consumption (Enter doesn't also click),
overlay `Ignored` → globals, Esc ladder, DataScope reactions (§5.4 table), CLI/IPC inputs
(`--play`, `.m3u` file → onboarding), connection limits sent at startup and after refresh,
`ApplyConfig` sent on edits, reminders refresh; usable at 790 and 1600 pt at text size 12 and
16; [e2e after WP-06/08/09/01] every remaining `SKELETON-STUB` removed. Help is opened with
`HelpOverlay::with_contexts(<current context stack>)` (current contexts first, §13.11) instead
of `overlays::build(OverlayRequest::Help)`; after an Alt chord resolves, drop its text event; when
`layout::sidebar_width` is 0 call `widgets::sidebar::sidebar` in a horizontal top panel (glyph strip).

**WP-11 Live/Guide/Player UI** — [kittest with `TestUi` + seed + fake `EpgReader`] Live
3-pane/2-pane/stack layouts; Enter rule; preview autoplay off by default, keyboard-rest only,
never replacing a user-started stream; guide focus model per §13.4 (`EpgGridState::handle`),
digits jump, default collection, now-line, budgeted bucket loads; programme popover actions
(watch, catch-up, remind toggle); info banner vs controls focus (arrows always zap); OSD
legibility with `osd_accent` in a light theme; mini player + toast avoidance; side panels;
numeric entry; tracks overlay with sub delay/size; Edit information; [bench `benches/guide.rs`]
< 8 ms/frame median over 200 frames, 5 000 channels, release.

**WP-12 Home/VOD/Settings/Onboarding** — [kittest] onboarding steps incl. failure routing, Guide
step, server normalization, "Open Live TV" after the channels phase; Home rails + Get started
rail; poster grid navigation and type-to-filter; detail pages (`Detail` context, seasons,
end-of-playback); search overlay sections + actions; every setting in §13.9 editable, persisted
and applied live; source edit/delete/reorder; EPG priority reorder by keyboard and drag;
favourites lists and reminders pages; parental PIN set/unlock/relock; EPG mapping assign/clear;
logs viewer + export; [e2e after WP-04/06/10b] onboarding against the mock panel; every state of
§13.12 reachable in tests or the manual checklist.

**WP-13 Integration, performance & release** — [bench, M2 release] first frame < 300 ms with a
100k-channel DB; library load < 400 ms; guide < 8 ms/frame; 100k M3U end-to-end < 5 s; 415k
programmes end-to-end < 8 s; [manual] the §23 checklist (theme switch, tiled layouts, media
keys, idle, other-workspace 60 s, 1-hour soak with RSS ± 10 %, real providers); `makepkg` and
`desktop-file-validate` on the final tree; removes leftover lint allows; finalizes docs.

### 24.3 Integration order
0. **WP-00** (this revision): B2–B5 and M1–M11 of the implementability review applied, plus
   the UX and architecture fixes that touch frozen contracts → commit + tag `skeleton-v1`.
1. Phase 1 in parallel: WP-01, 02, 03, 04, 05a, 07, 08, 09, 10a. First merges: the milestone-A
   test-support PRs of WP-04 (mock panel) and WP-07 (media), then WP-10a before WP-11/12 start
   real UI work.
2. Phase 2: WP-05b (after WP-02), WP-06 (after 02/03/04/05a), WP-10b (after 06 and 08; 01/09
   as they land).
3. Phase 3: WP-11 and WP-12 in parallel.
4. Phase 4: WP-13.

---

## 25. Risks & verification backlog

| Risk | Mitigation / owner |
|---|---|
| Hidden-workspace playback (no frame callbacks → no `logic`) | `render_unfocused` window rule shipped; controller video watchdog (`vid=no` after 2 s, resumed on the next render); everything correctness-critical off the UI thread; vsync off + `pre_present_notify` pacing so the UI thread never blocks in a swap and keeps answering pings (no ANR dialog, §11.1). WP-07 + WP-13 measure 60 s on another workspace with and without the rule (audio, `frame-drop-count`, CPU). |
| Render-thread rule (UI calling mpv) | `Player::mpv()` removed; UI gets only `render_core()` for context creation; controller-only `Arc<Mpv>`; D2. |
| Text shaping (RTL, Indic) in egui | Fallback fonts + reshaper/bidi for labels (WP-01); accept Indic limitation; revisit if egui adds shaping. |
| Provider connection limits | One stream, stop-before-start waiting for END_FILE, zap delay, budget fed from stored account info, preview autoplay and guide preview off by default (§11.5). |
| Judder without VRR / report_swap | Documented; measured (WP-07); user `mpv_options` escape hatch. |
| EPG import time / writer stalls on big guides | Generations + per-batch FTS rows + chunked sweeps (no transaction larger than one batch) + back-pressure + bench gates (WP-02/05a/06/13). |
| FTS drift | Contentless FTS deleted by rowid before programmes; no cascades into `programme`; AUTOINCREMENT ids; maintenance cleanup; store tests after re-imports. |
| UI-thread DB time costing video frames | 2 ms `FrameBudget`, `NowNextCache`, budgeted `ProgrammeCache`, 4 MiB image uploads/frame. |
| Keymap/focus complexity | Single table source, raw-input consumption, tests, generated help + hint line (WP-10a). |
| Lost overrides / progress on refresh | Store-computed fingerprints on every user table + reattach + orphans UI (WP-02/12). |
| Idle inhibitor via foreign display | WP-08 verifies with Omarchy's idle service; window-rule fallback documented. |
| CJK font memory | Regular-weight faces, one file per collection, setting to disable fallbacks. |
| Software decode cost on Asahi | `hwdec=no` default here; one stream; preview autoplay off; video suspended when not rendered. |
| Spike gaps (audio, fullscreen, HDR) | WP-07 / WP-13 manual checklist items. |
| Parallel WPs blocked on each other | Phase 0 safe defaults, seams (`MediaResolver`, `PlayerStore`, `EpgReader`, `Fetch`, `TestUi`, seeders), milestone-A test-support PRs, phased order (§24.3). |

---

## 26. Review decisions (round 2)

Everything not listed here was accepted as proposed and is reflected in the sections above and
in the skeleton code. Listed: points rejected or implemented differently, with the reason.

**UX / Omarchy / UHF review**
* *M2 — screen-specific contexts for Jobs, Logs, Orphans*: instead of three new contexts,
  `Delete` is a shared `Remove` item action (cancels the highlighted job in the Jobs overlay,
  removes from a favourites list, cancels a reminder), the Logs level filter lives in the Ctrl+S
  "Sort & view options…" menu, and Orphans use the Panel idiom (Enter = attach, Delete/x → menu
  with Forget). `EpgMapping` and `NumericEntry` contexts were added as proposed. Fewer contexts,
  same "no raw keys" guarantee.
* *Minor 2 — `Screen::reserved_corner()`*: the mini-player rect is owned by `VideoHost`, so the
  app passes it to screens as `UiCtx::mini_rect` and to toasts as an avoid rect. Same effect
  without a new trait method that every screen would have to implement.
* *Minor 3 — `osd_accent` threshold "≥ 3:1 against #000"*: that rule keeps the `white` theme's
  `#6e6e6e` (4.1:1 against pure black) although it is unreadable over video. We measure against
  the scrim composited over mid-grey video (`#2b2b2b`), which rejects it.
* *Minor 7 — Compact navigation*: accepted; the 48 pt rail now extends down to 480·rem (layout
  code), the glyph strip below that is WP-10a work (not in the skeleton).
* *Minor 11 — `SortMode::Custom` and per-group `user_order`*: rejected for v1.
  `SortMode::Provider` already means "provider order with user reordering" and is labelled
  "Custom order" once something was reordered; Alt+↑/↓ under another sort switches to it with a
  toast. Per-(group, item) ordering would change the override key and the backup format; the
  per-item limitation is documented (§8.3, §13.2) instead.
* *M6(e) — drag & drop / `tuner file.m3u`*: reuse the onboarding card
  (`Route::Onboarding(PlaylistFile)`), so `JobKind::ImportPlaylistFile` (suggested by the
  implementability review M8) was removed — `AddSource` covers file sources. Dropped XMLTV
  files are added as guides (Phase 5). Caveat: drops depend on the eframe/winit backend and
  were not confirmed to arrive under Wayland/Hyprland (MANUAL_CHECKLIST §8).

**Architecture / concurrency review**
* *B1(d) — video watchdog*: implemented (not left optional), because users who don't install the
  Hyprland snippet would otherwise burn CPU decoding invisible video.
* *M2 — entry ids*: the controller reads `playlist/0/id` right after the synchronous `loadfile`.
  If WP-07 ever observes a race, switch to `mpv_command_ret` and read `playlist_entry_id` from
  the command result (same `Input::Loaded` contract).
* *M3 — resolve failures*: nothing is written to the failure cache for the requested item
  either: resolve errors are deterministic (missing row, DRM, catch-up window) and the item's
  `ItemRef` isn't available when resolution fails. The resolver keeps a session skip set so
  zapping steps over the channel, and the UI shows `PlayerEvent::Failed`.
* *M5 — "limit ≤ 1 or unknown"*: accepted literally, so M3U sources (unknown limit) also stop
  before start on same-source zaps; the cost (~0.1 s per zap) is accepted for safety.
* *M7 — stale EPG links*: chose the "rebuild the Library on `DataScope::Epg`" option instead of
  a separate `channel_links` patch path (one code path, < 400 ms off-thread).
* *M8 vs implementability M11 — where tvg-shift is applied*: merged into one rule: store reads
  return EPG time and UI callers apply `ProgrammeSlim::shifted` once; only `now_next` takes the
  shifts (it must query at `now − shift`) and returns shifted results keyed by (channel, shift).
* *Minor 9 (shared-cache tests)* and *implementability M7*: `Store::open_in_memory` was replaced
  by `Store::open_temp` (a real WAL file) rather than documented as single-threaded.

**Implementability / WP plan review**
* *B1 — commit*: the rule (Phase 0 ends with a commit on `main` and tag `skeleton-v1`; frozen
  changes re-tag `skeleton-v1.N`) is in §24.1, but this revision deliberately commits nothing —
  the commit and tag are the first action after it is accepted.
* *B3 — safe defaults*: applied more broadly than listed: integration `start` functions return
  `Err`, `hypr::focus_self` and `ipc::try_handoff` are no-ops, `JobManager::submit` reports
  `Finished(Err)`, and the in-memory `LiveIndex`/`VodIndex`/`FavIndex` accessors are fully
  implemented (every Live/guide/VOD frame calls them).
* *B5 — `TestUi` returning a `Harness`*: `egui_kittest` is a dev-dependency and therefore not
  visible from `src/` when integration tests compile the library, so `TestUi` owns every
  `UiCtx` part and tests build their own `Harness`. The proposed closure seam for the guide was
  replaced by the equivalent `EpgReader` trait, which `NowNextCache` shares.
* *M2 — favourites out of the snapshot*: `Library::favs` is kept as the build-time copy that the
  app adopts on every swap (so `RebuildLibrary` stays one job); screens only use `UiCtx::favs`.
* *M6 — fixture sizes*: the generator's default XMLTV is now 1 235 channels × 7 days × 48 =
  414 960 programmes, matching the 415k gate (the old default produced 1.7M).
* *Minor 2 — clippy baseline*: brought to zero in Phase 0 (not deferred) by scoping the
  `allow(dead_code, unused_*)` to stub modules in `lib.rs`; each WP removes its own.
* *Minor 10 — `save_progress(final_)`*: `PlayerStore::save_progress` takes no flag; final saves
  are guaranteed by the Stop/EndFile/switch effects plus the joined writer at shutdown. WP-07
  verifies with a recording `PlayerStore`.

**Implementation decisions (phases 2–3, recorded in WP-13)** — folded into the sections named:
* Job coalescing is stricter than "while Queued/Connecting": a request made after the running
  job read its source row runs again in full (§5.2).
* `tuner --refresh` does nothing while a windowed instance runs (§9.5, §21.4).
* Help and Jobs close when requested again; switching screens closes overlays (§13.11).
* Back on a lone sub-page goes to the parent screen; Ctrl+P works only while something is
  playing (§15.1).
* `file://` inputs are treated as paths (§21.4).

---

## 27. Phase 5: Xtream auto-upgrade, M3U movies/series fallback, free-channel presets

Base: tag `p5-base` on `tuner-v1` (this section + the skeleton of §27.9). Where this section
disagrees with an earlier one, this section wins; the earlier text is superseded only where
named (§13.10 step 2 "offered as Xtream"; §9.1 step 4 "every entry is a channel"; §8.1 gains
the M3U series key).

### 27.1 Decisions (user, authoritative)
1. **Free channels.** Onboarding's Type step offers "Try free channels" next to Xtream / M3U
   URL / M3U file, and Settings ▸ Playlists has "Add free channels". Choices: iptv-org for the
   user's country (default), iptv-org all countries, Free-TV. Nothing is fetched until the user
   picks one; nothing is ever added on its own.
2. **Auto-upgrade.** An M3U that is really an Xtream account (a `get.php` URL, or entries that
   are Xtream stream URLs — also local files) is switched to the Xtream API without asking.
   The user can opt out ("Keep as M3U") in the onboarding summary and in the source's settings.
3. **Existing sources** upgrade silently on their next refresh, with a toast; user data carries
   over; switching back is in the source's settings; a failed upgrade leaves the source M3U.
4. **Fallback.** When a playlist stays M3U, Xtream-style `/movie/` entries become movies and
   `/series/` entries become series episodes; plain playlists are unaffected; M3U movies have
   only title / poster / group and their detail pages must look intentional.

### 27.2 Detection (`sources::xtream::detect`, `sources::m3u::detect_xtream_url`)

**Playlist URL** (`m3u::detect_xtream_url`, existing): http(s), last path segment `get.php` or
`player_api.php` (case-insensitive), non-empty `username` and `password` query values
(percent-decoded). **Change:** the server keeps the path prefix before that segment
(`https://h.example/sub/get.php?…` → server `https://h.example/sub`), because panels behind a
prefix serve `player_api.php` and the stream paths under it; `urls::*` builders already append
to `server` verbatim. The existing test's `https://h.example` expectation changes accordingly.

**Stream URL** (`detect::parse_stream_url`):
```
scheme://host[:port][/prefix…]/{live|movie|series}/<user>/<pass>/<id>[.<ext>][?query][#frag]
```
* scheme http/https; host lowercased; userinfo, query and fragment ignored;
* the kind segment is the 4th from the end, compared case-insensitively; `<user>`/`<pass>`
  are non-empty and percent-decoded; `<id>` = the last segment's stem before the first `.`,
  all ASCII digits, parsed as `i64` > 0; `<ext>` = the rest after that `.` (lowercased), `None`
  when absent;
* `server` = `scheme://host[:port]` + the prefix segments joined with `/` (no trailing `/`);
* not matched: `timeshift/…`, the short live form `host/<user>/<pass>/<id>` (too ambiguous —
  any CDN path `a/b/123.ts` would qualify), anything with fewer segments. The short form counts
  only with the login of the full stream URLs: `PlaylistScan` accepts short-form entries whose
  two segments are that login (and gives up on any other pair), so a panel playlist whose live
  section uses the short form still upgrades; `m3u_to_xtream_moves` maps such channels to
  `xt:live:<id>` (`detect::short_live_id`).

**Whole playlist** (`detect::PlaylistScan`, fed every entry URL in file order): the playlist
is one Xtream account iff
1. at least one entry is a stream URL, and every stream URL carries the same (username,
   password);
2. every entry that is not a stream URL is served by a `host[:port]` that also serves stream
   URLs (provider extras such as an info clip are allowed; an entry on a foreign host means the
   playlist is a merge and upgrading would lose entries → no upgrade).

`push` returns `false` as soon as rule 1 or 2 fails (plain playlists stop at their first
entry, so the scan is free for iptv-org / Free-TV). `finish` returns the credentials with
`server` = the most frequent stream-URL server (first seen on ties — load-balanced panels
list several hosts; the API is tried on one).

**Redaction.** Credentials found here are registered with `redact::register_secret` (user and
password) by the caller *before* any log line or error can contain them; detection itself never
logs. Log lines about an attempt name the source and `redact_url(server)` only. Upgraded sources
store the login in `xt_*` (registered at startup by `register_secrets`); the M3U URL / file is
kept (§27.3). Stream URLs in playlists were already covered by the Xtream path-segment rule.

### 27.3 Upgrade flow

**New source (probe, `jobs::probe`).** A draft with `settings.keep_m3u == true` is probed as M3U,
no attempt. Otherwise:
* M3U URL that is a `get.php` URL → authenticate (as today). Success → `ProbeResult { kind:
  Xtream, xtream: Some(login), account, upgraded_from: Some(M3uUrl), … }`.
* Any other M3U URL / file → download + count as today, feeding `PlaylistScan` from
  `count_entries`; if it qualifies → authenticate (+ live categories estimate). Success → the
  same Xtream result with `upgraded_from: Some(kind)`.
* **An upgrade failure never fails the probe**: auth rejected / account unusable / network /
  HTML → `debug!` (redacted) and the M3U result (or the M3U's own error) stands. For a `get.php`
  URL that means the probe then downloads the playlist like any M3U URL.

Onboarding (Test step) for `upgraded_from.is_some()`: "Xtream login found — movies, series,
catch-up and account info included." with the account lines, and buttons **Continue** (focused)
· **Keep as M3U**. Continue → `AddSource` with `kind: Xtream`, `xtream: login`, and the original
`url` / `path` KEPT (it is what "switch back" uses); "Keep as M3U" → the M3U draft with
`settings.keep_m3u = true` (then the Guide step rules for M3U apply). The Details step's old
"Use the Xtream login" row (`Field::UseXtream`) goes away: the probe decides.

**Existing source (refresh, `jobs::import_source::refresh_source`)**, under the per-source lock,
after reading the row:

| Source row | What the refresh does |
|---|---|
| M3U, `keep_m3u` | M3U import with the §27.5 fallback. No attempt. |
| M3U, not `keep_m3u` | Upgrade attempt if **due**, else M3U import (+ fallback). |
| Xtream with `m3u_origin()`, `keep_m3u` | **Switch back**: `set_source_kind(origin)`, then a forced M3U import with the Xtream→M3U key moves (§27.4). |
| Xtream (native, or upgraded and not `keep_m3u`) | Xtream import as today. |

*Due* ⇔ the refresh is forced (Settings ▸ Refresh, the switch toggle) OR there is no failed
attempt on record OR the last one is ≥ 7 days old — ≥ 1 hour when the server couldn't answer
(network error, timeout, HTTP 5xx or 429: `transient`, which says nothing about the login —
also when the login worked but every list failed only that way; a rejected login, an unusable
account or a bad payload keep the week). The hour is a floor, not a timer: the M3U import of
the same refresh usually succeeds and the source is then not due before `playlist_refresh_h`,
so the check normally comes with the next scheduled refresh (or Ctrl+R / *Refresh now*). Failed attempts are
recorded in `meta` as `xtream-upgrade:<source id>` = `{"at": <unix s>, "login_failed": bool,
"transient": bool}` (deleted on success and on switching).

Attempt:
1. Credentials: from the URL for `get.php` (before any download); otherwise download the
   playlist — **ignoring ETag/Last-Modified when no attempt is on record**, so the first refresh
   after this release cannot be a 304 — or read the file, and run `PlaylistScan` over it on the
   blocking pool (early exit). No credentials → M3U import of the same file.
2. Register the secrets; `authenticate` with the source's User-Agent. Rejected / unusable
   account / network / bad payload → record the attempt, `info!` "Couldn't use the Xtream login
   of <name> (<redacted reason>); keeping the playlist", continue with the M3U import (a
   `get.php` source downloads its playlist now). The refresh's own outcome is the M3U import's.
3. Success → compute the M3U→Xtream key moves (§27.4) from the current rows, then run the Xtream
   import with an *upgrade hook*: in the writer closure of the **first** `finish_catalogue` of
   that import (live, or VOD when live failed) and before it: `apply_key_moves`,
   `set_source_kind(Xtream, Some(login))` (clears validators), delete the meta marker. That
   closure is the commit point: before it, nothing user-visible changed; after it, the source
   is Xtream. `set_account` as usual; `notify(DataScope::Sources)`; `RefreshResult.upgraded =
   true`.
4. The Xtream import fails before any finish (every kind failed — nothing was written, since
   kinds fail at fetch time) → no commit; record the attempt; `info!`; run the M3U import from
   the same file in the same job. A cancel before the commit → no commit, no sweep (as today).
5. After the commit the refresh succeeds (`upgraded = true`, so the toast is shown) whatever
   follows: a kind failing fatally (a VOD list answering 401) is recorded in `last_error` like
   any partial failure and the remaining kinds are skipped; a cancel returns at once (no
   sweep, no bookkeeping — the source stays due). Moves of the kinds not finished are not
   lost: every Xtream refresh of an upgraded source (`m3u_origin()`) first recomputes
   `m3u_to_xtream_moves`, which finds user data still on M3U keys whose rows are still there,
   and applies them with the finishes of their kinds.

The outcome is `JobOutcome::SourceRefreshed { upgraded, account, … }`; the app shows
`Toast::info("Upgraded to Xtream login — movies & series now available")` whenever `upgraded`
(silent refreshes from the scheduler included) and forwards `account` to the connection budget
as before. The commit itself notes the source in `meta` (`xtream-upgraded-unseen:<id>`,
`note_unseen_upgrade`, in the finish transaction with `set_source_kind`); the app deletes it
when it shows the toast (`forget_unseen_upgrade`). Whatever is left — `tuner --refresh` has no
window, and a window that quit (or a headless run that timed out) cancels the job before its
outcome is shown — the next window's start takes (`take_unseen_upgrades`: sources still Xtream,
once; named when there are several playlists). `AddSource` runs the same refresh inline (an "Add anyway" M3U may upgrade during its
first import; onboarding's Done step shows the counts, no toast, and `add_source` drops the
`xtream-upgraded-unseen` note).

**Switching in Settings ▸ Playlists ▸ source** (UI): a toggle row "Use the Xtream login"
(= `!keep_m3u`) appears for Xtream sources with `m3u_origin()` and for M3U sources with
`keep_m3u` (a playlist the user kept; for other M3U sources upgrading is automatic and the row
is hidden). Changing it saves `SourceSettings.keep_m3u` (`set_source_settings`) and submits
`RefreshSource { force: true }` once that write landed (`JobManager::submit_tracked`, which
says whether the returned job runs the request or it runs again after that job); the refresh
performs the switch. Only that run answers the toggle: a forced refresh already running read
the playlist still kept, so its outcome is ignored. If the user asked for the Xtream login and
that run does not upgrade (`upgraded == false`, or it failed), the source is kept again and,
for an outcome without `upgraded`, the screen toasts "Couldn't sign in with this playlist's
Xtream login — still using the playlist" (the reason is in the log / `last_error` is untouched
by a failed attempt).

**Store guard.** `store::sources::update_source` must never null the Xtream login of a row whose
stored kind is `xtream` (keeps the stored `xt_*` when `src.xtream` is `None`): a source editor
opened before a silent upgrade would otherwise wipe the new login on Save. The UI also reloads
its editor copy on `DataScope::Sources`.

### 27.4 Identity across the switch (`store::import`)

Keys never change meaning (§8.1); user data is *moved* between keys with `KeyMove { from, to }`
(cross-kind capable `apply_key_moves`; holders as `move_user_data`, plus: a move to another kind
rewrites the `kind` column of `fav_item`, `item_override`, `watch_progress`, `history`; reminders,
manual EPG mappings and `stream_failure` rows are only moved channel→channel and otherwise
dropped; existing rows of `to` win). Moves are computed by the importer, applied inside the
finish transaction BEFORE `reattach`, and only for keys that hold user data.

* **M3U → Xtream** (`m3u_to_xtream_moves`, from the rows present before the import): every
  channel / movie / episode row of the source whose `StreamLocator::Url` parses as a stream URL →
  `live` ⇒ (Channel, `xt:live:<id>`), `movie` ⇒ (Movie, `xt:vod:<id>`), `series` ⇒ (Episode,
  `xt:ep:<id>`) — exact, and it also moves legacy channel-kind data of movie/episode entries to
  the right kind. Episode targets may not exist until the series' details load: the moved rows
  keep their fingerprint and are not orphaned meanwhile (finish never reattaches episodes of an
  Xtream source).
* **Xtream → M3U** (`xtream_to_m3u_moves`, from the M3U rows just written, before finish): every
  M3U row with `provider_id` (= the Xtream id from its URL, §27.5) → move (kind, `xt:…:<id>`) →
  (kind, `m3u:…`).
* **Legacy M3U channels reclassified by the fallback** (no Xtream involved): M3U movies and
  episodes keep the key the entry had as a channel (same `KeyAllocator`, same base key, file
  order), so the finish pre-step "kind fix-up" moves (Channel, k) → (Movie|Episode, k) for user
  keys of kind Channel absent from `channel` but present in `movie` / `episode` of the source.
  Xtream keys are namespaced (`xt:live:` vs `xt:vod:`), so this never fires for Xtream sources.
* **Group overrides** (`group_override` is keyed by group key, not item): after a finish that
  completed movies/series, for every `channel:X` override of the source with `locked` or
  `hidden` set, the same flags are copied to existing `movie:X` / `series:X` groups that have no
  override. Locks never get weaker by reclassifying (a locked "Adult" group of M3U movies stays
  locked as a movie group, and as the Xtream category of the same name — `get.php` writes
  category names as `group-title`). Renames, order and home pins are not copied.
* Anything not covered falls back to fingerprint reattach (§8.3); e.g. a provider extra entry
  (§27.2 rule 2) that Xtream doesn't list becomes an orphan.

Preserved: favourites, renames/hidden/locked/catch-up offsets, watch progress, history,
reminders and manual EPG mappings of live channels, parental locks of groups. Lost: the stream
failure cache of moved channels (by design), reminders/EPG mappings that were attached to an
M3U VOD entry seen as a channel (meaningless for a movie), group renames/order across kinds.

### 27.5 M3U movies & series fallback (`sources::m3u_vod` + `jobs::import_source`)

**Classification** (`m3u_vod::classify`, per entry): `parse_stream_url(url)` is `movie` ⇒ Movie,
`series` ⇒ Episode, and the entry has **no entry-level HTTP headers and no DRM** (movies and
episodes are played with the source User-Agent only — no `headers` column; an entry that needs
its own headers stays a channel so playback can't regress). Everything else ⇒ Live. Plain
playlists have no such URLs and are unaffected. Applies to every M3U import (also when an
upgrade failed or the user kept the playlist).

**Movies:** `name` = title (tvg-name when empty); `(clean_name, year)` = `clean_vod_name` (smart
names on) / raw name; `poster` = tvg-logo; groups `movie:<group-title>` (`;` lists, cleaned like
channel groups, "Uncategorized" fallback); `provider_id` = the Xtream id; `url_key` =
`identity::url_key(url)`; `stream` = `Url`; key = the entry's channel key (§27.4); rating /
plot / genre / cast none; `provider_order` = entry order.

**Episodes:** marker grammar (`parse_episode_title`, case-insensitive, leftmost match, title
first then tvg-name):
* `S(\d{1,4})[ ._-]*E(?:P)?[ ._-]*(\d{1,4})` — `S01E02`, `S01 E02`, `S1E2`, `S01.E02`,
  `S01_E02`, `S01-E02`, `S01 EP02`, `s01e02`;
* `\b(\d{1,2})x(\d{1,3})\b` — `1x02` (word boundaries keep `1920x1080` out);
* `Season\s*(\d{1,4})\s*[,.-]?\s*Episode\s*(\d{1,4})`.
Series title = the text before the marker, trimmed of whitespace and `- – | : . _ , ( [`; when
empty, the entry's first group-title. Episode title = the text after, trimmed likewise; `None`
when empty (the UI shows "Episode N"). Season 0 (specials) and episode 0 are kept. No marker
→ series = the cleaned title, season 1, episode = 1-based order among that series' unmarked
entries.

**Series:** one per `normalize_name(series title)` per source → key
`identity::m3u_series_key(norm)` (`m3u:s:<16 hex>`); `name` = first-seen series title,
`(clean_name, year)` via `clean_vod_name`; `cover` = first episode's tvg-logo; groups = union of
its episodes' `series:<group-title>` in first-seen order; `provider_order` = first appearance;
`provider_id` none. Episodes: key = the entry's channel key; `provider_id` = Xtream id; `stream` =
`Url`; `still` = tvg-logo when different from the cover; ordered by (season, episode, file order);
duplicates of one (season, episode) are all kept. Seasons are synthesized from the episodes.

**Writing.** Channels and movies stream in 2 000-row batches as today (groups first seen per
batch). Episode entries are collected compactly during the parse (a series index, numbers,
title, URL, logo, key, groups) and written after it as `ImportSeriesEpisodes` batches of ~2 000
episodes (whole series per batch) through `write_series_episodes`, which upserts the series,
upserts its episodes (`ON CONFLICT(source_id, key) DO UPDATE`, incl. `series_id`), deletes
episodes of that series not in the batch, rebuilds its seasons and sets `details_at` =
`imp.started_at`. M3U imports finish with `completed = [Channel, Movie, Series]` (a playlist that
dropped its movies sweeps them; series sweep cascades episodes), then reattach Episode user data
over the whole source (M3U episodes are complete at import, so missing ⇒ gone). Memory: one
compact record per episode entry until the end of the parse (~250 B each; 300k episodes ≈ 75 MB
peak) — acceptable; noted in §27.11.

### 27.6 Playback of M3U movies/episodes (`sources::resolve`)
Unchanged code path: `StreamLocator::Url` → the URL as-is; headers = `merge_headers(default,
source)` (source User-Agent → config default); title / subtitle / progress / resume as for
Xtream. Entries needing their own headers stay channels (§27.5), so nothing is lost. A disabled
source still refuses to play.

### 27.7 Detail pages without Xtream metadata (UI)
`LoadMovieDetails` / `LoadSeriesDetails` are submitted only for Xtream sources (as today);
for M3U sources the page never shows a skeleton (the series has `details_at` set at import; the
movie page must not treat `details_loaded_at == None` as loading for non-Xtream sources). Sparse
layout, theme tokens only:
* Movie: poster (or the generic poster placeholder) left; title; one meta line from what exists
  — year · category (primary group name) · source badge; no plot block, no "Cast:" line, no
  empty rating; the buttons row ([Play]/[Resume]/[Start over], favourite, watched) stays where
  it is on rich pages so focus order is identical; a muted hint "No description in this
  playlist" in the plot's place (Body small, `text_secondary`).
* Series: same header; season tabs and episode rows as usual; an untitled episode reads
  "Episode 2"; no duration column when unknown; progress/watched glyphs as usual; "opens at the
  next unwatched episode" unchanged.
* Pages of upgraded sources whose movie rows are still M3U-born (the movie kind failed on the
  upgrade) are Xtream pages: `jobs::details` already falls back to `provider_id`, which §27.5
  fills with the Xtream id, so details load.

### 27.8 Free-channel presets (`sources::presets`, implemented in the skeleton)
* URLs: iptv-org country `https://iptv-org.github.io/iptv/countries/<cc>.m3u` (`cc` = ISO 3166
  alpha-2 lowercased, **GB → `uk`**), all countries `https://iptv-org.github.io/iptv/index.m3u`,
  Free-TV `https://raw.githubusercontent.com/Free-TV/IPTV/master/playlist.m3u8`.
* Country: `presets::detect_country()` (cached `OnceLock`, two small local reads): the system
  timezone's IANA name looked up in `/usr/share/zoneinfo/zone.tab`, else the territory of
  `LC_ALL` / `LANG` (`en_GB.UTF-8` → GB; `C`/`POSIX` → none). Timezone first: many systems keep
  `en_US` wherever they are. Display name from `iso3166.tab` (`country_name`).
* `presets::presets(country)` → [IptvOrgCountry(cc) if known, IptvOrgAll, FreeTv], each with
  `url()`, `source_name()` ("iptv-org · United Kingdom", "iptv-org · All countries", "Free-TV")
  and `description()`.
* Onboarding: Type step gains "Try free channels" → a Presets step (choice rows, `[Modal]`);
  picking one builds an M3U URL draft (name = `source_name()`, glyph/colour defaults) and
  continues with Test → (Guide skipped when the list carries `x-tvg-url`) → Importing → Done.
  A preset whose URL is already a source's URL shows "Added" and is not selectable. Settings ▸
  Playlists gets "Add free channels" → `Route::Onboarding(OnboardingStart::FreeChannels)`,
  starting at the Presets step (Esc there leaves, like `AddSource`).
* Nothing is fetched before a pick; Free-TV's `x-tvg-url` (epgshare01) becomes provider EPG
  sources through the existing `url-tvg` path.

### 27.9 Skeleton API (tag `p5-base`)
Shared, compiled from the start (bodies `todo!("P5-sources")` only where nothing calls them):
* `model::SourceSettings.keep_m3u: bool` (serde default false) and `model::Source::m3u_origin()
  -> Option<SourceKind>` (implemented).
* `jobs::ProbeResult.upgraded_from: Option<SourceKind>` (probe sets it on today's `get.php`
  path); `jobs::JobOutcome::SourceRefreshed.upgraded: bool` and
  `jobs::import_source::RefreshResult.upgraded: bool` (false until the sources track lands).
* `sources::xtream::detect::{XtreamStreamUrl, parse_stream_url, PlaylistScan::{push, finish}}`
  — todo.
* `sources::m3u_vod::{EntryClass, EpisodeMarker, classify, parse_episode_title}` — todo.
* `sources::presets::{Preset, presets, iptv_org_code, detect_country, country_from,
  country_name, IPTV_ORG_ALL, FREE_TV}` — implemented + tested.
* `sources::identity::m3u_series_key(norm) -> ItemKey` — implemented, golden-pinned.
* `store::import::{ImportSeriesEpisodes, KeyMove, write_series_episodes, apply_key_moves}` and
  `store::sources::set_source_kind` — todo.
No migration: the schema is unchanged (series `details_at`, provider ids and `meta` cover it).

### 27.10 Work split (three parallel tracks + leak investigation)

File ownership is disjoint; a cross-track edit needs the owner's agreement and is listed in the
merge note. Everyone rebases on `p5-base`. Lean builds: clippy (check) + the tests you touched;
the integrator runs the one full gate.

**sources track** — owns `src/sources/**`, `src/jobs/**` except `scheduler.rs` and
`maintenance.rs`, `src/store/**` except `backup.rs` and the stale-source queries of
`store/sources.rs` (`stale_sources`, `stale_epg_sources`), `src/services.rs`,
`tests/{m3u,resolve,import_e2e,e2e_flows,xtream}.rs`, additions under `tests/support/**`.
1. `detect::parse_stream_url` + `PlaylistScan` (§27.2). Tests: URL shapes incl. prefix, port,
   case, percent-encoded creds, query/fragment, non-matches (timeshift, short form, zero/non-digit
   id); scan accepts one-account + same-host extras, rejects foreign host / second login, picks
   the most frequent server.
2. `m3u::detect_xtream_url` keeps the path prefix (update its test).
3. `m3u_vod::parse_episode_title` + `classify` (§27.5). Tests: every grammar variant, leftmost
   match, separators trimmed, empty series → group fallback (in the converter), `1920x1080` not
   a marker, headers/DRM keep an entry live.
4. M3U converter: movies, episode collection, series assembly, shared `KeyAllocator`,
   `provider_id`/`url_key`, groups per kind; `write_series_episodes`; finish with all three
   kinds + episode reattach + kind fix-up + group-override carry-over (§27.4–27.5). Tests (in
   `tests/import_e2e.rs`): a provider-style M3U imports N live / M movies / S series with the
   right seasons; a legacy DB where those entries were channels with favourites/history/locked
   group keeps them on the new kinds; plain playlist import unchanged (counts + keys).
5. `apply_key_moves`, `m3u_to_xtream_moves`, `xtream_to_m3u_moves`, `set_source_kind`, the
   `update_source` guard (§27.3).
6. Upgrade flow in `refresh_source` / `refresh_m3u` / `refresh_xtream` (hook at the first
   finish, attempt marker, forced first download, fallback to M3U in the same job), `notify`,
   `RefreshResult.upgraded`; probe upgrade for entry-shaped playlists, `upgraded_from`, the
   "never fails the probe" rule; `keep_m3u` honoured everywhere. Tests (mock panel): get.php
   source upgrades on refresh with favourite/progress/reminder carried to `xt:` keys and
   `upgraded == true`; entry-shaped local file upgrades; auth rejected → stays M3U, imports, marker
   written, no second attempt within 7 days unless forced; `keep_m3u` → no request to
   `player_api.php`; switch back restores M3U keys for favourites; Xtream import failing at
   fetch → M3U import, source still M3U.
7. `tests/support/mock_xtream.rs`: `get.php` also emits `/movie/` and `/series/` entries
   (m3u_plus style, `Show S01 E02` titles) and an optional path prefix.

**ui track** — owns `src/ui/**`, `src/keymap.rs`, `src/app.rs`,
`tests/{onboarding,guide,widgets,keymap,parental}.rs`.
1. Onboarding: "Try free channels" choice + Presets step, `OnboardingStart::FreeChannels`,
   "Added" state; drop `Field::UseXtream`; Test step copy + "Keep as M3U" for
   `upgraded_from`; upgraded draft keeps `url`/`path`; Keep → `keep_m3u` M3U draft (§27.3, 27.8).
   Tests: preset pick builds the right draft (country + all + Free-TV); Keep as M3U builds an M3U
   draft with `keep_m3u`; Continue builds an Xtream draft that keeps the URL.
2. Settings ▸ Playlists: "Add free channels"; per-source "Use the Xtream login" toggle (rules of
   §27.3) → save + forced refresh + failure toast; editor reloads on `DataScope::Sources`;
   source kind line shows "Xtream (from M3U)" for upgraded sources. The server field of an
   upgraded source is not re-normalized unless edited (it may carry a path prefix).
3. App: toast on `SourceRefreshed { upgraded: true }` (test in `app.rs`, extend the existing
   refresh-toast test).
4. Movie / series detail pages for M3U items (§27.7): no skeleton for non-Xtream sources, sparse
   layout, "Episode N" titles. Test: an M3U movie page shows no loader and no empty plot/cast
   rows (one `widgets`/screen test, extend existing ones).

**fixes track** — owns `src/platform/**`, `src/player/**`, `src/main.rs`, `src/cli.rs`,
`src/ipc.rs`, `src/config.rs`, `src/store/backup.rs`, `src/jobs/{scheduler,maintenance}.rs`, the
stale-source queries of `src/store/sources.rs`, `tests/{ipc,player_mpv,store_backup,e2e_soak}.rs`.
Its own fix list (not part of this section) plus, for phase 5: backup/restore keeps an upgraded
source's M3U URL/path and `keep_m3u` (settings JSON already round-trips; verify the URL of an
Xtream-kind source is exported and the restore Merge matching still pairs it).

**leak track** — investigation only; any minimal fix lands as a cross-track edit in the owning
track's files, listed in that track's merge note.

### 27.11 Risks
* **Losing entries on upgrade**: guarded by §27.2 rule 2 (foreign hosts block the upgrade).
  Provider extras on the panel host are dropped by design (reattach may orphan their data).
* **Parental bypass by reclassification**: group locks carry to the new kinds (§27.4); item
  locks move with the item. Tests must cover the locked-group case.
* **Editor race wiping a new login**: `update_source` guard + editor reload (§27.3).
* **Path-prefixed servers**: `normalize_server` strips paths; only the onboarding Xtream form
  and an *edited* server field may call it.
* **Upgrade storms**: one `player_api.php` login per due refresh at most; failures back off 7
  days; `keep_m3u` stops them entirely.
* **Episode memory** in huge fallback playlists (§27.5) — bounded by the playlist size; revisit
  with a temp table if a real 1M-entry list shows up.
* **First-refresh download**: existing M3U URL sources re-download fully once (no 304) to be
  scanned — one extra download per source, then normal conditional refreshes.
