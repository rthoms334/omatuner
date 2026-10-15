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
24 Implementation plan (work packages) · 25 Risks & verification backlog · 26 Review decisions ·
27–29 Xtream upgrade, category filter, several playlists · 30 Agent control (MCP) · v0.2 batch:
31 Guide fixes · 32 Up next · 33 Following & news flash · 34 Spoiler guard · 35 Multi-view ·
36 Reminders & sleep timer · 37 Claude plugin & skills · 38 Playlist codes & library load ·
39 Connecting AI agents · 40 Casting to a TV · 41 Tuner's updates

---

## 1. Product overview & v1 scope

**Tuner** is a native IPTV player for Omarchy (Hyprland). It targets the feature set users love
in UHF (Apple-only) — fast live TV with a real guide, catch-up, VOD with resume, global search,
smart reconnects — and fixes UHF's documented gaps (hidden channels, orderable EPG sources,
stale guides, configurable skip, subtitle delay, stream info, profiles-friendly storage). It
*looks and behaves like Omarchy*: keyboard-first (launcher idiom in lists, vim keys where no
text is typed), square corners, JetBrains Mono Nerd Font, colours taken live from the current
Omarchy theme, foreground-alpha state tints that work in light themes.

* Binary / Wayland `app_id` / Hyprland class / MPRIS suffix / `.desktop`: **`tuner`** (a
  launch straight into the pop-out maps as `tuner-popout`, §21.6).
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
| Event draining in `ui()` | Moved to dedicated threads (D2). **Correction (review round 2):** eframe's `logic` does NOT run while the window is on a hidden Hyprland workspace — winit-wayland reports no visibility and `RedrawRequested` needs a compositor frame callback, which Hyprland withholds from hidden workspaces. Mitigations: the `render_unfocused` window rule (frame callbacks at `misc:render_unfocused_fps`, default 15) and the controller's video watchdog (`vid=no` after 10 s without a rendered frame, resumed when the UI renders again) — §11.1. **Correction (hidden-workspace ANR):** with eframe's default vsync the UI thread did not merely idle there — it **blocked** in Mesa's `eglSwapBuffers` (swap interval 1 waits for the previous frame's callback), stopped answering `xdg_wm_base` pings, and after `misc:anr_missed_pings` Hyprland showed "application not responding" while mpv played on. Now vsync is off and frames are paced by winit frame callbacks (`pre_present_notify` in `App::ui`): hidden, the UI thread sits in its event loop and answers pings, with or without the window rule (§11.1). |
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
  → UI swaps the snapshot and its `FavIndex`, hands the old snapshot and the search index to
  the memory maintenance thread (§5.4);
  `JobOutcome::SourceRefreshed { account }` → `PlayerCommand::SetConnectionLimit`.
* **Play**: UI `PlayerCommand::Play{media, zap, origin, immediate}` → controller resolves
  `MediaRef → Playable` with its own reader → `PlayerMachine` (Zapping → Loading) →
  (stop-before-start if same limited source) → `loadfile url replace -1 opts` → entry id read →
  events → Playing → snapshot published (+ repaint); UI paints the FBO texture wherever the
  current layout puts the video.
* **Media key while hidden**: Omarchy `omarchy-shell media next` → MPRIS (tokio) →
  `PlayerCommand::Zap` → controller resolves/loads; UI not involved. Without the
  `render_unfocused` rule no frames are rendered; after 10 s the controller sets `vid=no`
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
| `ui/scope.rs`, `ui/widgets/scope_bar.rs`, `ui/overlays/playlists.rs` | §29 | Which playlist a screen shows: eligibility, the stored scope, look-alikes and free colours; the scope bar / button / stripe; the Alt+P picker and a tab's menu. |
| `agent/**` (`proto`, `shared`, `server`, `tools`, `client`, `mcp`) | §30 | Agent control: IPC v2 protocol, the running app's handler, the `tuner mcp` stdio MCP server and its tools. |
| `devshot.rs`, `examples/demo_profile.rs`, `scripts/screenshots.sh` | dev tooling | README screenshots: env-only capture driver, made-up demo profile, capture script (§23.1). |

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
      Onboarding; else `general.start_screen`, Home by default) →
      RebuildLibrary job (async; lists show skeletons until it lands) → CLI/IPC request
```
First frame target: < 300 ms after launch regardless of library size (library loads async;
measured by WP-13 with a 100k-channel DB).
An error that ends `main` (exit status 1) is reported on stderr, in `tuner.log` and — while a
windowed launch has no window yet — as a desktop notification (§21.7).

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
`load_with(conn, &LoadOptions)` (the job passes `LoadOptions::from_config`: audio languages and
the explicit-content `Visibility`, §20.3) also builds `VodIndex::stacks` — the copies of one
title in "All playlists" (§29.11).

A build reads ONE snapshot: it opens a read transaction on its connection (or reads in the
caller's transaction, when one is open). The channels are read on a thread of their own, on a
second read-only connection (`store::open_reader`), while the first one reads the movies and
series. The second connection's read transaction must be on the same snapshot. It reads
`PRAGMA data_version` before the first connection's transaction starts and again when its own
starts. The value only changes when another connection committed in between. If it changed,
the build tries again (3 times), then reads everything on the first connection. Without a path
(an in-memory database) or inside a caller's transaction, the build also uses one connection.
This saves about 100 ms on a real-size catalogue (`benches/startup.rs`).

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
| VodDetails, Progress | screens react in `on_event` (details that bring a new TMDB id also submit `RebuildLibrary`: ids decide stacks, §29.10) |

A change of `player.audio_langs` submits `RebuildLibrary` (it picks every stack's top copy).

**Memory upkeep** (`platform::memory`, field report: 1.1 GB after a day of 4K HEVC in software
with a 460k-item library). glibc keeps freed heap resident unless it is at the top of an arena,
and this app frees in bursts: a replaced snapshot (~2 M allocations, ~240 MiB at that scale), the
global search index (~110 MiB), mpv's decoder frames (hundreds of MiB at 4K 10-bit). One
"tuner-mem" thread (`Services::memory`) drops what the UI hands it (`release`: the old snapshot
on every swap — dropping it in the frame took 20–400 ms — and the search index) and calls
`malloc_trim(0)` afterwards, after the player stops, switches video off (`vid=no`) or (re)loads
a stream (it follows the player's snapshot channel), and every 5 min; triggers coalesce (2 s
settle, ≥ 10 s apart; `malloc_trim` itself takes 0.1–50 ms). The search index exists only while
the search card is open (rebuilt in the background when it opens; closing the card cancels a
collection still running). Screens resolve keys only for what they look up (Movies/Series:
titles with progress or in Favourites; Home: favourited titles), never a map of every title,
keyed on the snapshot's VOD lists (`Library::vod_identity`: a zap's copy shares them, so a zap
rescans nothing; a progress save that brings no new key only remaps).
`platform::memstats::MemStats` (RSS, anon, swap, peak, `mallinfo2` in use / free) feeds the
10-min health line in the log. Measured on the field library with a 4K HEVC stream (8 min,
second half): RssAnon 1121 → 742 MiB on average, peak RSS 1311 → 1017 MiB; with video off
after a trim the process sits at ~320–430 MiB; 4K playback has a floor of ~700 MiB (the
library ~240 MiB + mpv ~450 MiB). Not done on purpose: `shrink_to_fit` (frees no pages),
capping decoder threads, a fixed mmap threshold.

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
per-stream caches (by channel key, not user data, not in backups): stream_failure ·
  channel_video (measured picture size, + fp so it follows a re-keyed channel)
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

### 6.2.1 `0003_quality.sql` (quality badges, §13.0)

```sql
-- added by the migration's Rust step only when missing (the migration is safe to re-run)
ALTER TABLE channel ADD COLUMN quality INTEGER;   -- name's quality word: 1 SD 2 HD 3 FHD 4 4K
ALTER TABLE channel ADD COLUMN family  TEXT;      -- with a quality word: normalize_name(cleaned name)
CREATE TABLE IF NOT EXISTS channel_video (        -- what mpv decoded when the channel last played
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  item_key  TEXT    NOT NULL,
  name      TEXT    NOT NULL,                     -- channel.name (raw provider name) when measured
  url_key   TEXT,                                 -- channel.url_key when measured
  width     INTEGER NOT NULL,
  height    INTEGER NOT NULL,
  at        INTEGER NOT NULL,
  PRIMARY KEY (source_id, item_key)
) WITHOUT ROWID;
```
* `channel.quality` / `channel.family` are written by every import (`ImportChannel.quality` =
  `NameQuality::of` the `smart_names` parse of the provider name, whether or not smart names
  are on). `family` is `normalize_name` of the smart-cleaned name, so "BBC One SD" and
  "UK: BBC One 2160p" share it where identity's `norm_name` (whose quality-tag list keys depend
  on, so it stays) does not; NULL (no quality word) reads as `norm_name`. Databases from
  before 0003 get both from the stored names by the migration's Rust backfill (same
  transaction, once).
* `channel_video` is written by the player controller through the writer
  (`PlayerStore::record_video` → `store::live::record_video`) once per channel and size, only
  while a **live** channel is `Playing` (a zap or a reconnect can still show the previous
  picture, so the controller re-reads `video-params` at PLAYBACK_RESTART rather than trust
  mpv's event order; catch-up archives may be encoded differently). No `DataScope` is sent:
  the UI keeps this session's measurements itself (`ui::data::MeasuredQuality`, cleared when
  a new library lands — it has them, and a refresh may have moved keys) until the next build.
* Keyed by identity like `stream_failure`, so refreshes keep it — but **valid only while the
  channel at that key still has the provider `name` (and `url_key`, when both are known) it was
  measured under**: the quality duplicates of an M3U channel share a base key (`normalize_name`
  drops the quality words) and are numbered by playlist position (`KeyAllocator`: `K`, `K#2`),
  so a reordered or grown playlist moves keys between them, and a renamed channel ("BBC One
  HD" → "BBC One FHD") must not keep the old size. After a completed channel import, a row
  whose key is gone or no longer matches is moved to the one channel of the source that has
  its name (several: the one that also has its `url_key`) unless that channel has its own
  measurement, else dropped — never an orphan. Two duplicates that swapped keys both follow.
* `library::load` reads both (`LEFT JOIN channel_video` on key, name and url_key);
  `ChannelSummary.quality` = the measured class if any, else the name's; none for radio (a
  cover picture is no video). Class of a decoded `w×h` (`Quality::of_video`): the
  better of what the height (≥ 2000 4K, ≥ 1000 FHD, ≥ 700 HD, else SD) and the width (≥ 3800,
  ≥ 1900, ≥ 1260) say, so cropped 1920×800 and anamorphic 1440×1080 read FHD. **1440p is
  FHD**: four classes keep the badge narrow, and QHD is closer to FHD than to the 4K it would
  otherwise claim.
* **Migration numbering**: `user_version` is positional. This migration was written as
  `0002_quality` alongside `0002_category_filter` (§28) and renumbered to `0003` when both
  merged (with its `backfill` name match). It is safe to re-run, and a database from a
  pre-merge build of this branch (v2 with the quality columns but no `grp.raw_name`) gets the
  category-filter columns from `migrations::reconcile_renumbered` before `0003` re-runs.

### 6.2.2 `0004_name_rules.sql` (quality-label trust §13.0.1, re-cleaned names §9.6)

```sql
CREATE TABLE IF NOT EXISTS quality_check (       -- trust evidence: one row per measured channel name
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  name      TEXT    NOT NULL,                     -- channel.name (raw provider name) when measured
  claimed   INTEGER,                              -- channel.quality then (NULL = no quality word)
  measured  INTEGER NOT NULL,                     -- Quality::of_video of the decoded size
  at        INTEGER NOT NULL,
  PRIMARY KEY (source_id, name)
) WITHOUT ROWID;
```
* Written next to `channel_video` by `store::live::record_video` (same trigger: a live,
  non-radio channel `Playing` at a known size), as an upsert: a channel measured again
  replaces its row, so zapping one channel fifty times is one channel of evidence and a
  claim that later plays right stops counting as wrong. Keyed by **name**, not item key: the
  evidence is about the provider's label, and it outlives refreshes that move keys between
  quality duplicates. It is never swept by imports (a removed channel's evidence still says
  how the provider labels); it goes with its source. Not user data (no fingerprint, not in
  backups).
* `store::live::label_trust` aggregates it per source (`GROUP BY source_id, claimed,
  measured`) into `model::LabelTrust` at every `library::load`.
* The migration's Rust step seeds it from `channel_video` (claim = the quality word of the
  name each size was measured under) and runs `migrations::reclean_names` (§9.6).

### 6.2.3 `0005_explicit.sql` (explicit content behind parental control, §20.3)

The SQL file is comments only; its Rust step (`migrations::backfill_explicit`) adds the
columns when missing (safe to re-run) and fills `grp.explicit` from the stored names:
```sql
grp.explicit            INTEGER NOT NULL DEFAULT 0 -- import's finding: 0 no, 1 name, 2 provider is_adult
channel.explicit        INTEGER NOT NULL DEFAULT 0 -- the provider flags the item (Xtream is_adult)
movie.explicit          INTEGER NOT NULL DEFAULT 0
series.explicit         INTEGER NOT NULL DEFAULT 0
group_override.explicit INTEGER                    -- user: NULL as detected, 1 marked, 0 "Not explicit"
```
* Backfill: `sources::explicit::is_explicit_name(COALESCE(raw_name, name))` → 1 for groups
  still at 0 (a provider's 2 is never lowered). Provider flags arrive with the next refresh.
* Every import rewrites `grp.explicit` (`group_code(adult, raw_name)`) and the items' flag;
  the user's word lives in `group_override` (keyed by group key: survives refreshes, backed up,
  carried to `movie:X` / `series:X` with the locks when entries are reclassified, §27.4 —
  only "Mark as explicit", never "Not explicit").

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
architect sign-off. A migration whose new columns need data SQL can't compute gets a Rust
`backfill` step that runs in the same transaction right after its script (0003: channel
quality and family from the stored provider names, ~0.1 s per 100k channels, once; 0004:
trust evidence from `channel_video` and every display name re-cleaned from the stored provider
names — 0.75 s once for 79k channels + 302k movies + 78k series, measured on the names of a
real playlist in memory; only changed rows are written).

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
(LRM); free pages given back; `PRAGMA optimize`; `wal_checkpoint(TRUNCATE)`.

**Free space.** The database uses `auto_vacuum=INCREMENTAL`. A guide refresh writes the new
generation beside the old one, so the file briefly holds two guides; with `auto_vacuum=NONE`
it never shrank back (a user's 690 MB file was 270 MB free pages). After every generation sweep
and in `Maintenance`, `store::release_free_pages` runs `PRAGMA incremental_vacuum` in 8 MiB
steps (one short write transaction each) until nothing is free. New databases get the mode
before their first table. 0.3.2 converted existing databases once, as `Store::open` opened them
(one `VACUUM`: 14 s for that 690 MB file, leaving 389 MB). Later builds dropped the conversion
(user decision, 2026-10-15), so a database made before 0.3.2 that never ran 0.3.2 keeps
`auto_vacuum=NONE`: `incremental_vacuum` does nothing there and its file stays at its peak.

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

### 9.6 Smart names (`sources::smart_names` on `sources::name_tokens`)
Display-only cleanup of provider names (UHF "Smart Playlists"; per source
`SourceSettings.smart_names`, default `general.smart_names`, on). The raw name stays in
`name` / `grp.raw_name`; identity (`identity::normalize_name` / `normalize_group`, frozen) is
unaffected, so toggling or changing the rules never changes keys. The quality word is taken
from the name whether or not cleanup is on (`channel.quality`, §6.2.1).

**One tokenizer** (`name_tokens::tokens`) is shared by channel, VOD and category display
names and by category tag detection (§28.2), so `"UK ★ BBC One"`, `"UK ❖ BBC One"`, `"|UK|
BBC One"` and `"UK: BBC One"` mean the same everywhere. Tokens: **words** (letters, digits,
marks of any script; `+ & ' ’` anywhere and `! %` after a word character — `E! News`, `Sky
Sports+`, `AT&T`, `+1`, `100%`; joiners between word characters: `- – . / _`, and `: ,`
between digits — `Sci-Fi`, `Canela.TV`, `24/7`, `15:55`), **separators** (a run of any other
punctuation or symbol, Unicode P* / S* via `unicode-general-category`, private-use icon
glyphs: `| : - ★ ✦ ❖ ◉ ● • ▎ ┃ ⓧ # » ⚽`), **decoration** (superscript / modifier /
small-capital letters and digits, split off a glued word: `TF1ᴴᴰ`, `⁴ᵏ`, `ᴿᴬᵂ`, `³⁸⁴⁰ᴾ`,
`ᶜᶦᵗʸ`), **flags** and **brackets** (`( [ { ⁽ 【 「`). Any whitespace (NBSP included) only
separates; each token remembers whether a space preceded it, so kept text is put back as
written.

**Rules** (`clean_channel_name`, `clean_vod_name`, `clean_group_name`):
1. *Leading prefixes*, repeatedly, after decoration and flags: `[…]` (any tag), `(XX)` /
   `|XX|`, and one to three *prefix words* followed by any separator run (`UK:`, `UK |`,
   `US ★`, `UK ❖`, `PL VIP:`). A dash separates only spaced after upper-case codes (`DE -
   Das Erste`, never `Al - Jazeera`); `. , / ' _` never do (`No. 1`). Prefix words: a
   country / language / region of the category tag tables (§28.2: `UK`, `ALB`, `EXYU`,
   `LATAM`, `HINDI`, `URUGUAY`), a known three-letter code (`COUNTRY3`), a service tag (`VIP
   SAT RAW PPV 24/7`), a quality word; and, as the only word, any two letters (`RK:`, `ZG:`:
   provider line-up codes, as before), code lists (`BE/FR:`) and a code with a short
   line-up label (`IN-PREM:`, `BE-VIP:`, `AR-SUBS -`). Unknown words stop it: `CNN: Live`,
   `AT&T: …`, `MY TV: …`, `PRIME: …` stay. Something that names the channel must follow (not
   only quality words): `"UK: HD"` → `UK`. Movie / series titles take upper-case codes only
   (`America: The Motion Picture`, `It: Chapter Two` are titles). A quality word of the
   prefix counts only when the name has none after it (`4K: beIN SPORTS 6 ᴴᴰ` is HD).
2. *Quality words anywhere* → `Quality`, best wins: `SD 480p 576p HD 720p FHD 1080p 1080i
   1440p QHD UHD 4K 2160p 3840p` (1440p / QHD are FHD like a measured 2560×1440). `8K` /
   `4320p` are format words, no claim: providers put them on event slates and placeholders
   (`- NO EVENT STREAMING - | 8K EXCLUSIVE | …`, 8.4k names of the real playlist), never on an
   8K picture.
   glued to symbols (`4K◉`), in `(…)` / `[…]` / `⁽…⁾`, or as decoration (`ᴴᴰ`, `ꜰʜᴅ`, `ᶠ ᴴᴰ`
   — a run of decoration is read joined first).
3. *Tags* (stripped into `CleanName.tags`): codec / format words (`HEVC H.265 x265 HDR 50fps`,
   `⁵⁰ FPS`, `8K`), the upper-case `RAW` feed tag of channels (not after `WWE` / `NIGHT`;
   titles keep `Raw Deal`), any `[…]`, superscript groups (`⁽ᴮᴷ⁾`), `(…)` groups
   that are tags (`backup`, `multi-sub`, `geo-blocked`…; `(FHD@60FPS)` is quality + format),
   superscript text (a superscript country code, `ELEVEN ᴾᴸ`, is also the country). Other
   `(…)` groups are part of the name (`BBC One (Scotland)`, `(EAST)`); stylised small
   capitals that are no known word stay (`ꜱᴘᴏʀᴛꜱ 1`).
4. *Ends*: separators, flags and stray brackets at either end are dropped (`◉`, `★`, `|`,
   `:`, `##`; a `#` glued to a word stays: `#0`, `#12 Texas A&M vs …`); the full stop of an
   abbreviation stays (`T.I.M.`). Inside the name everything stays as written (`Parijs-Tours:
   Mannen`, `NEXT | Lyon vs Nice | 15:55`, `Sp⚽rts`), except that two separators left next
   to each other become the stronger one (`STREAMING - | 8K EXCLUSIVE |` → `STREAMING |
   EXCLUSIVE |`).
5. VOD: a trailing `(YYYY)` / `[YYYY]` / `- YYYY` (1900–2100) is the year. Groups: a quality
   word written as plain text is what the category is about and stays (`UK ❖ SPORTS 4K` →
   `Sports 4K`, `UK ❖ 4K` → `4K`, `4K| Relax` → `4K Relax`; superscript ones are decoration);
   a full country name before text in another script stays (`Lebanon ▶ لبنان`); all-caps
   names are title-cased (acronyms kept). Never empty: nothing left → the trimmed raw name.
6. *Collisions*: names cleanup made identical although the provider names differ get what
   it dropped and not all of them share, after ` · `
   (`smart_names::distinguishing_suffixes`). Categories of one playlist and kind at import
   (`store::import::distinguish_group_names`, also in migration 0004: `GENERAL ᴴᴰ/`,
   `GENERAL ʰᵉᵛᶜ`, `General` → `General · HD`, `General · HEVC`, `General`); channels of one
   category with the same badge at `library::load` (display only: `FI: TV5` / `FI: TV5 ᴿᴬᵂ`
   → `TV5` / `TV5 · RAW`; `PL:` / `AF: CANAL+ SPORT 5` → `… · PL` / `… · AF`; renamed
   channels keep the user's name). Stream info (`I`) shows the provider's name of a live
   channel (`Provider …`) whenever it differs from the displayed one.

Measured on a real 79k-channel Xtream playlist (names only, never committed): the decorations
listed in the user report (`★` prefixes 20k names, `VIP:` 2k, `◉` 1.5k, superscripts, glued
`4K`) are gone; what stays are unknown line-up labels (`PRIME:`, `GOLD:`, `DSTV ★`, `Jawy ★`)
and `|` between the fields of event names. 79k names clean in ~80 ms (release). Channels of
one category with the same cleaned name and badge although the provider names differ: 602
sets after cleaning, 183 after the collision suffixes (rule 6; the rest are the provider's
own duplicates: `4K` / `UHD`, `HD` / `HD (720P)`).

**Existing data.** Names are cleaned at import, and every refresh upserts `clean_name` /
`grp.name` (with `grp.raw_name`), so a refresh applies new rules. Databases from before these
rules get them at once from migration 0004's `reclean_names` (no refresh needed): per source,
smart names = its own setting, else inferred from its rows (the app-wide default lives in the
config file, not the database) — on when any stored display name differs from the provider's;
it rewrites changed `clean_name`s (channels, movies, series) and `grp.name`s (where
`raw_name` is stored), and every channel's `quality` / `family`. A later rule change appends a
migration whose backfill calls `reclean_names` again. Display names are not recomputed at
library load: that would cost every start (~80 ms per 79k channels) for something that changes
only with a new build.

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
player's `RenderClock`; the controller tick sets `vid=no` after 10 s without a rendered frame
while Playing (10 s, not less: decoding restarts mid-GOP on resume, and HEVC shows seconds of
broken picture until the next keyframe, so a quick workspace switch must not suspend) (audio continues, `snapshot.video_suspended = true`), and the app sends
`PlayerCommand::ResumeVideo` (`vid=auto`) as soon as `logic` runs again. Nothing else that must
work while hidden lives on the UI thread (§3.1). **Verification (WP-07, repeated by WP-13)**:
60 s on another workspace with and without the rule — audio continuity, `frame-drop-count`,
CPU, video resumes; fullscreen toggling; 1-hour soak; HDR/10-bit sample.

### 11.2 Event pump & controller (implemented)
`events::EventPump` observes properties (pause, paused-for-cache, cache-buffering-state,
core-idle, time-pos, duration, demuxer-cache-duration, volume, mute, track-list (JSON), seekable,
sub-delay, sub-scale, audio-device, current-ao, video-params/w,h,
hwdec-current, aid, sid, eof-reached), requests `warn`
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

| Failure | Live (first play) | VOD / catch-up |
|---|---|---|
| Network, Timeout, Stalled, StreamEnded (EOF on live), HTTP 5xx, other mpv | backoff 0.5 · 1 · 2 · 4 · 8 · 15 · 15… s, max `reconnect_max_attempts` (8) | backoff, max 3, **resume at last position − 2 s** |
| HTTP 401/403 | 1 retry after 3 s, then Error "account expired or too many connections"; inside the **race window** every 3 s (below) | same |
| HTTP 404/410 | 1 retry after 2 s, then Error "channel offline" | Error |
| Provider message instead of a stream (`ProviderError`, below) | 3 retries 3 s apart, then Error "The provider can't deliver this channel right now" + the provider's text | — |
| Other 4xx, unsupported format, DRM, resolve error, budget | Error immediately | Error |

**A live channel that played keeps reconnecting** (field report 2026-10-11: restreamer outages
of minutes and connection-limit races ended hours-long playback for good, and dimmed the
channel). Once the current live target reached Playing, every failure except DRM, resolve,
budget and unsupported format reconnects with no attempt limit: the backoff above, its 15 s
step repeating for about 5 minutes after the first failure, then every 30 s (about 120 requests
an hour while a channel stays down); refusals, 404s, other HTTP errors and provider messages
wait at least 3 s. Nothing is recorded in the failure cache. The overlay reads "Reconnecting…
attempt n" (never "(n/0)") with an obvious **Stop** button (`q`; `Ctrl+Shift+S` anywhere).

**Connection-limit race window.** Xtream panels keep counting a closed connection for a while
(reproduced: HLS sessions held 30 s, TS connections 10 s), so on a source whose limit is 1 or
unknown a zap is often refused at first. When a *different* stream of the same source closed
less than 45 s before a Play's first load — or the *same* channel after it had played (stop,
then restart it) — a 403 is retried every 3 s until 45 s after that close; the next 403 is
final (Error, recorded). The window is armed once per Play from that close — never by the
target's own failed attempts, which never played — so a channel the provider really refuses
still reaches Error. Race retries, the one 401/403/404 retry, provider-message retries and the
network backoff each count on their own: a long window doesn't leave a first open that then
times out without its 8 attempts.

**Provider messages.** A restreamer that lost a channel answers the playlist address with 200
and a line of text ("Cannot read /home/…/streams/123_.m3u8"). mpv's playlist parser reads the
headerless `.m3u8` body line by line and redirects to an "address" made of each line. On that
redirect of a live target the controller reads the inserted entries; one with whitespace is
prose, not an address: it stops mpv at once (often before the bogus address is requested) and the
machine fails the load as `ProviderError` with the redacted text.

**HLS keep-alive off.** `demuxer-lavf-o=http_persistent=0` by default (a user `mpv_options`
value replaces it): providers close idle keep-alive connections between segment fetches
("Error reading HTTP response: End of file" every segment) or leave them half-dead (a
`network-timeout` stall per fetch and skipped segments). One TCP handshake per 2–10 s fetch
costs nothing.

**Logging.** Every state change of the current target is logged at INFO with its source id,
item key and the time spent in the previous state (`player: source 2 xt:live:717661 Playing →
Reconnecting(attempt 1, StreamEnded) after 1h12m03s; retry in 0.5 s; cache 0.0 s`), plus
"recovered after n attempts in t" (attempts of that outage) / "gave up after …"; END_FILE
reasons of our file; HTTP statuses (≤ 5 per status per 10 s, then a summary); a provider
message at WARN when it first appears (DEBUG while it repeats); app, mpv and FFmpeg versions at
start; and every 10 minutes a health line (state, cache, `cache-speed`, dropped frames, FFmpeg's
own reconnects, hwdec, RSS, swap, peak and glibc heap in use / free). Titles are not in INFO
lines (explicit titles never reach the log, §20.3): a DEBUG line names the title once per new
target. Stream addresses never appear.

HTTP statuses (parsed from mpv/ffmpeg log lines) classify a failure only while **Loading**
(they are cleared on PlaybackRestart): a storm of HLS segment 404s while Playing is a Network
failure (backoff), not "channel offline". Attempts reset after 30 s of stable playback. mpv's `stream-lavf-o reconnect*` covers TCP
drops inside a file; the policy covers everything that ends/stalls the file (HLS segment 404
storms, token expiry, server kicks). Overlay: "Reconnecting… (2/8)" (or "attempt n") with
countdown; `r` retries now; Backspace = last channel; Stop.

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
* **Casting** (§40): the lease is held for the provider connection the relay uses, so the budget is
  unchanged: one open lease, and a zap or reload on the TV follows the same stop-before-start
  (the session waits up to 1.5 s for the upstream close). While a cast runs the Wayland idle
  inhibitor (§21.2) is released, so the screen may lock, and a logind *sleep* inhibitor
  (`cast.keep_awake`) keeps the relay alive instead; closing the lid still suspends.
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
reconnect_streamed=1,reconnect_on_network_error=1,reconnect_delay_max=5`,
`demuxer-lavf-o=http_persistent=0` (§11.4; `mpv_options` can set it back), `user-agent`,
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
│ 󰄙     │  ← Play on (only with a TV, §40)                                            │
│ 󰍉     │                                                                             │
│ 󰋜     │                                                       ┌ toast ────────────┐ │
│ 󰔂     │                                                       └───────────────────┘ │
│ 󰃰     │                                                       ┌ mini player ──────┐ │
│ 󰎁     │                                                       │  video 16:9       │ │
│ 󰟴     │                                                       └ BBC One · ✕ ──────┘ │
│ 󰒓     │                                                                             │
├───────┴─────────────────────────────────────────────────────────────────────────────┤
│ ↵ play · ⇧↵ fullscreen · Ctrl+F fav · Shift+F10 more · ? help   Guide updated 2 h ago│  ← status bar 26·rem
└─────────────────────────────────────────────────────────────────────────────────────┘
```
Content padding = panel-padding (18). Panes separated by 1 px `pane_divider` (fg @ 0.28).
Status bar: left = the focused / hovered item with its playlist's full name when cards or rows
show playlist codes ("Northwind · 2019 · ★ 7.9", §13.0), then running jobs ("󰑐 Refreshing My
IPTV · 45 %", click = Ctrl+J) or, when idle, the per-context hint line generated from the keymap
(`keymap::hint_line`); right = guide
freshness, offline marker, and — when the mini-player is hidden (Compact or disabled) — a
now-playing chip (click = player, ✕ = stop). **No clock** (the Omarchy bar already shows one).
Toasts stack bottom-right ABOVE the mini-player rect (`VideoHost::mini_rect`), which screens
also receive as `UiCtx::mini_rect` (the guide keeps the focused cell clear of it). The
mini-player never shows over Onboarding or the Player (`allows_mini_player() == false`).
Overlays centre with a bg @ 0.5 scrim.

### 12.3 Video-only layout (pop-out, `ui/popout.rs`)
The window shows only the player — no sidebar, status bar or mini player — while
* our pop-out is requested or active (`o` / Ctrl+Shift+O, §21.6), whatever its size, or
* something is playing or paused and Hyprland reports the window **floating and pinned** and
  it is at most Omarchy's Super+O pop size, 1300 × 900 logical px
  (`omarchy-hyprland-window-pop`'s default). A bigger pinned window, or Super+O with nothing
  playing (pinning the browsing UI), keeps the full UI; floating alone never switches.

It never shows while a pop-in is in flight, nor between the pop-in's result and Hyprland's
unpin event (no flash). The app draws the Player screen and gives it the keys whatever the
route stack says (`TunerApp::view_route`); the stack itself stays, so popping back in returns
to the screen the user left (it is entered again). The video-only player is not a route of its
own: a route step back in video-only mode only removes a `Player` route on top of the stack,
never the screens below. Only user keys pop the window back in: `p` (keep playing) and `q`
(stop and close) leave the player and pop in; `f` pops in and then goes fullscreen (a pinned
window can't); a finished movie returning to its page stays popped out ("Nothing playing")
with the page waiting underneath. Key stack: the player's contexts with `PopOut` right above
`PlayerLive`/`PlayerVod` — Esc pops back in once a side panel, number entry or controls focus
has had it. The compact OSD strip (title · volume, what's on or the position, "o · Esc back"
when at least 360 px wide) shows for the usual OSD time after popping out.

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
* **Quality badges** (Settings ▸ Interface ▸ Quality badges, default on, applied live):
  `SD` / `HD` / `FHD` / `4K` after the channel name — Live rows, the guide's channel column and
  side panel, Search results, the player's channel list / mini guide / zap banner, Home channel
  tiles (favourites rails included). Source: what mpv decoded the last time the channel played
  (`channel_video`; this session's plays at once via `ui::data::MeasuredQuality`), else the
  quality word of the provider name (`channel.quality`) — so "BBC One 4K" that plays at 1080p
  shows FHD, and an untagged channel gets a badge once it played (§6.2.1). A measured value and
  a name one look the same — two looks for one word is noise, and the player's OSD and the Live
  preview show the decoded value anyway (those show it whatever the setting) — **except a
  doubtful claim, `4K?` / `FHD?`** (§13.0.1). The badge's tooltip (Live preview, guide side
  panel) says where it comes from: "Played at HD (the provider says 4K)", "The provider says
  4K; not verified until it plays", and for `4K?` "…; not verified. Channels of this playlist
  labelled 4K often play at a lower quality." Radio channels have none. The badge follows the name and **stays when space runs out — the name
  elides beside it** (duplicates share a name, so the badge is what tells them apart); only
  when that would leave the name less than max(2 × badge, 6 em) does the name get the room
  (`widgets::badge::name_with_quality` / `job_with_quality`, also search hits). The zap
  banner's title badge shows until the channel plays; then the decoded badge on the right
  says it.
* **Playlist on cards** (Settings ▸ Interface ▸ Playlist on cards: **Auto** / Always / Never,
  applied live): every item says which playlist it comes from, from the snapshot's `SourceMeta`
  (user-chosen glyph + colour; no DB access per frame; `ui::data::SourceMarks::for_view`).
  **Auto** shows the playlist only while **two or more playlists are eligible for that kind
  of item** (`Library::eligible`: enabled and with channels / movies / series — a disabled
  playlist or a free-channel preset without movies doesn't count) **and the view shows all of
  them**: a scoped screen (§29) shows marks only in "All playlists"; views without a scope
  (Home, Search, detail pages, the Live preview, the player) count as All
  (`UiCtx::source_of(kind, source)`; scoped screens use `Scope::marks`).
  * **Playlist codes**: cards and rows show a playlist as a **short code chip** — 1–3 letters
    or digits in the playlist's colour inside a 1 px outline of that colour
    (`badge::CodeChip`) — never its name, so a long name can't push the year or a channel's
    name out. The code is the user's (Settings ▸ Playlists ▸ edit ▸ **Short code**,
    `SourceSettings::code`) or derived from the name (`sources::codes::auto`): the whole name
    when it has ≤ 3 letters and digits (`TV`, `TV 2` → `TV2`), else its words' initials
    (`Harbor Free` → `HF`, digits whole: `Channel 44` → `C44`), one word → the initials of its
    halves (`Northwind` → `NW`) or its first two letters (`Aurora` → `AU`). Codes are
    **unique** among all playlists (`sources::codes::assign`: the user's first, a duplicate
    gets the first free variant `TV2`, `TV3`, …, then letters), resolved once per library
    build into `SourceMeta::code`. The editor's field shows the derived code as placeholder,
    accepts letters and digits only (upper-cased, ≤ 3) and refuses another playlist's code
    ("Northwind already uses NW"). The full name stays one step away: the chip's tooltip, the
    item's accessible label, and the **status bar** while the item has the cursor or the
    pointer ("Northwind · 2019 · ★ 7.9"; `status_bar::note_focus`). Typing a playlist's code
    in a category / group filter finds its categories, like its name.
  * *Poster cards* (Movies / Series grids, every Home poster rail — Continue watching,
    favourites, Recently added): a **top-left code chip** (the lock / watched chips grow from
    the top-right, so they never collide), the height of those chips on bg @ 0.85; tooltip =
    the playlist's name. A folded stack card chips **up to three** of its playlists (its own
    first), then `+n` for the rest; fewer when the card is narrow (its own always shows;
    tooltip "Also in n more playlists"). The **meta line is "year · ★ rating" only**.
  * *Rows and tiles* (Live channel rows incl. the virtual Favourites / History / All groups,
    Live ▸ Reminders, Home channel tiles incl. Recently watched, the player's channel /
    history / mini-guide panels, search results — a chip per playlist of a stack): the code
    chip at the right edge (left of the lock on tiles), name as tooltip, the same in every
    density — the item's name gets the width.
  * *Group rows* (Live groups, Movies / Series categories) in All: the code chip right of the
    group name — the two "Sports" are told apart (replaces the old " · TV 2" suffix).
  * *Detail pages* (movie, series — Xtream pages too, not only playlist ones) and the Live
    preview: a source badge (glyph + name) leads the meta line. (Sparse playlist pages no
    longer append the name to the meta.)
  * *Player*: the top bar shows `● TV` left of the clock (name in its colour where it reads on
    the scrim) for channels, movies and episodes.
  * *Accessibility*: the item's accesskit label carries the playlist ("The Matrix · Provider
    A") on cards, tiles, rows and search hits.
  * *Contrast*: a source colour is used for text or a glyph only where it reaches 3:1 on its
    background (`badge::legible_on`; window bg for cards / rows, the OSD scrim in the player).
    Otherwise the code / name is fg and the colour moves to a non-text mark: the code chip's
    outline thickens to 1.5 px, the border of a badge — e.g. a yellow playlist on a light
    theme.
* **Reorder vs sort**: Alt+↑/↓ while the list is sorted by anything other than *Custom order*
  (`SortMode::Provider` = provider order with user reordering) switches it to Custom order with
  a toast "Switched to custom order". Ctrl+S opens a small sort & view menu (launcher card),
  never a blind cycle.

#### 13.0.1 Quality-label trust
Providers label channels 4K that play HD (a real playlist
labels about 5.7k of 79k channels 4K / UHD; its 8K labels are event slates and claim
nothing, §9.6).
Measured quality always wins. Per source,
`model::LabelTrust` counts the measured channels (`quality_check`, §6.2.2) and, per claimed
class, how many claims played lower. Once **≥ 3 channels of the source were measured
(`MIN_MEASURED`), ≥ 2 of them claimed the class (`MIN_CLAIMS`: one bad stream is no pattern)
and at least half of those played lower**, the source's *unmeasured* claims of that class
show `4K?` (`QualityBadge::doubtful`). Only 4K and FHD are judged (the classes that get
inflated; an HD claim that plays SD is rare and harmless). A claim that played *better* is
not wrong. A channel measured again replaces its evidence, so the doubt clears when the
claims turn out right. Computed at `library::load` (the doubt follows a measurement at the
next library build — a refresh or the next start — while the played channel itself shows
its measured class at once). Best-first ordering of duplicates puts a doubtful claim after
the class beneath it (`4K?` after any `FHD`, before `HD`). Evidence of channel
names the provider no longer lists is dropped at each refresh. The stream info panel (`I`)
adds a `Quality` line on live channels with a quality word: "Quality  named 4K · plays HD" /
"named 4K · as labelled" / "named 4K · not verified" before the size is known.

### 13.1 Component catalogue (Omarchy tokens → widgets)

| Component | Widget | Tokens |
|---|---|---|
| Menu/list row | `widgets::rows::menu_row`, `list::virtual_list` | height max(50, body + 2·row-padding-x), 58 with detail; label heading 16 Medium; detail body-small 11 @ 0.52; icon column 36 @ icon-large 18; chevron @ 0.36; cursor = fg @ 0.08 fill + **accent** text (+ optional 1 px fg @ 0.25); selected = fg @ 0.18 |
| Channel row | `rows::channel_row` | as menu row; number body @ 0.52; progress bar 3 px (track fg @ 0.18, fill fg); next programme body-small @ 0.52; quality badge after the name (§13.0) |
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
| Badge | `badge::*` | caption, 1 px border colour @ 0.6; LIVE = urgent; source = source colour (palette role) + glyph, text fg where the colour is < 3:1 on bg |
| Scope bar | `scope_bar::scope_bar` (tabs), `scope_bar::scope_button` (`● TV 2 ▾`), `scope_bar::paint_stripe` | control-height row; tab = dot (palette colour, non-text) · name (fg selected / hovered, `text_secondary` otherwise) · count (body-small @ 0.52); selected tab underlined 2 px in its colour (accent for All); hover fill fg @ 0.08; button = bordered button with the dot; stripe 2 px in the picked playlist's colour (§29) |
| Source mark | `badge::CodeChip` (poster corner chips, OSD lists), `badge::paint_code_chip` (rows, tiles, search hits, copy cards), `badge::source_badge` (detail pages, preview) | the playlist's code (caption) in its colour, 1 px outline in the colour, never narrower than tall; poster chips = bg @ 0.85, top-left, height of the lock chip; colour < 3:1 → fg code + 1.5 px colour outline; tooltip = playlist name (§13.0 "Playlist codes") |
| Quality badge | `badge::InlineBadge::quality` + `name_with_quality` (rows, grid, search, OSD lists), `badge::quality_badge` (preview, guide side panel) | monospace caption, text @ 0.52 (`text_secondary`; `text_disabled` on dimmed rows; OSD text in the player), 1 px border at 0.6 × the text's opacity, Md gap after the name |
| Logo / monogram | `logo::logo` | square, aspect kept; monogram on fg @ 0.04, text @ 0.52 |
| Poster card | `poster::poster_card` | 2:3; title body; meta body-small @ 0.52 (year · ★ rating; the playlist is a code chip, §13.0); progress accent; cursor 2 px accent outline; corner chips: playlist top-left, lock / watched top-right |
| Empty / error state | `empty_state::empty_state` | glyph display-large 28 accent @ 0.8; title 14 fg @ 0.7; detail @ 0.52 |
| Sidebar | `sidebar::sidebar` | bg dark_background; row control-height + md; selected fg @ 0.18 + accent; order [Play on] · Search · Home, Live TV, Guide, Movies, Series, Up next · Settings (Play on — where plays go, "This computer" / the TV's name in the accent colour, a click opens the chooser — is shown only while casting is on and a TV is ready, is a control and never the lit tab: cast design §3.9; a panel gap sets Play on, Search and Settings apart; same in the rail and the Compact glyph strip; Alt+digits stay bound to their screens) |
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
│ 󰍉      │ GROUPS            ⌕    │ UK News…                               │ ┌───────────────────────────────────┐ │
│        │ 󰓎 Favourites       42  │ ▣ 101 BBC One HD              󰓎 ▕██░▏ │ │                                   │ │
│ 󰋜 Home │ 󰓎 Sport (list)     11  │     News at Eight · 20:00–21:00        │ │          VIDEO  (preview)         │ │
│ 󰔂 Live │ 󰋚 History          12  │ ▣ 102 BBC Two                    ▕█░░▏ │ │                                   │ │
│ 󰃰 Guide│ All channels    9 812  │     Gardeners' World · next Panorama   │ └───────────────────────────────────┘ │
│ 󰎁 Movie│ ───────────────────────│ ▣ 103 ITV1  󰀦 (offline, dimmed)        │ BBC One HD · 101   [HD] [⟲ 7 d] [▣ Pr]│
│ 󰟴 Serie│ UK News           128  │ ▣ 104 Channel 4                        │ News at Eight          20:00 – 21:00  │
│        │ UK Sports          64  │ …                                      │ ▕█████████░░░░░░▏ 34 min left          │
│ 󰒓      │ Kids 󰌾             31  │                                        │ The latest national and international │
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
* **Opening group** (Settings ▸ Interface ▸ Live TV opens on, `interface.live_opens_on`): the
  first time Live TV opens in a session without a group or channel asked for (start screen,
  sidebar, Alt+2, Home's links), it shows **All channels** (default) in the current playlist
  scope, cursor on the first channel — or on the playing one. *Favourites* opens the first
  favourites list with channels in that scope, All channels without one. Coming back later
  keeps the group the user was in; back from the player follows the zapped channel; a route
  with a group or channel opens that. The guide's default collection follows the same choice.
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
* **Duplicates best first**: with the default sort (custom order), the SD / HD / FHD / 4K
  duplicates of one channel (same `channel.family`, same primary group) trade places so the best
  known quality comes first; they keep the slots they had between them, and every other
  channel — a duplicate of unknown quality, or one the user placed (`user_order`) — keeps its
  own (`LiveIndex::collection`, All channels and provider groups; favourites, history and
  search keep their order). The zap order is the list order, as everywhere.
* **Several playlists with channels** (§29): the scope bar runs across the top (tabs at Wide,
  `● TV 2 ▾` otherwise). One playlist picked: its groups, and All channels / favourites lists
  / History / Reminders narrowed to its channels — so are the zap lists (`zap_list_in`); the
  channel header reads `TV 2 › UK News`. In All, group and channel rows name their playlist in
  its colour (channel rows: its glyph at Compact density), and an open group's header names
  its playlist: `All playlists › Sports · TV 2`.

Medium (≈ 790): the preview stays on the RIGHT; groups become a drawer (← / Backspace on the
channel list — ← also while it filters, keeping the filter — or click the breadcrumb):
```
┌──┬────────────────────────────────┬─────────────────────────────┐
│󰍉 │ UK News ▾  (filter…)           │ ┌─────────────────────────┐ │
│󰋜 │ ▣ 101 BBC One HD      ▕██░▏    │ │      VIDEO (16:9)       │ │
│󰔂 │   News at Eight               │ └─────────────────────────┘ │
│󰃰 │ ▣ 102 BBC Two         ▕█░░▏    │ BBC One HD · 101 [HD]       │
│󰎁 │   Gardeners' World            │ News at Eight  20:00–21:00  │
│󰟴 │ …                             │ ▕██████░░░▏                  │
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
screen is Home (`StartScreen::Home`; an old `"auto"` loads as Home).
Rail scroll bars float without a reserved gutter. Mouse movement over an overflowing rail
shows its bar; it fades after about 1.5 s without movement or on keyboard input, even when the
pointer rests over the rail. A bar being dragged stays visible until release. Keyboard use
never shows rail bars. Horizontal touchpad swipes and Shift+wheel scroll within the rail;
hiding its bar preserves its scroll position.
Keyboard moves reveal the whole rail, including its header. ↑/↓ and PgUp/PgDn reach the
same page top/bottom as touchpad scrolling; Home/End reach the first/last tile within a rail.
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
  All. Default: the one last picked in the guide this session, else like Live TV's (§13.2
  "Opening group"): All channels; with *Favourites*, the first favourites list with channels in
  the playlist scope and not hidden by parental control, else All channels. ‹ › and Now are
  clickable buttons too.
* **Several playlists** (§29): the guide shows Live TV's playlist — a `● TV 2 ▾` button before
  the collection button (Alt+P) and the stripe under the header; rows, collections and zap
  lists follow it, and a group of another playlist falls back to the default collection. In
  All the picker lists each playlist's groups under the playlist's name (a heading row nothing
  chooses) and the header names the group's playlist ("Sports · TV 2").
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
* **Mouse**: wheel = rows, Shift+wheel / horizontal trackpad = time, a click on a cell =
  Enter there (focus it and open the programme popover; a gap watches), a click on the
  channel column = watch, right-click on a cell or a
  channel = focus it and open the channel menu (the menu key), hover tooltip (title, time,
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
**Several playlists with movies (series)** (§29): the scope bar sits above both panes; one
playlist picked narrows the categories, All, Continue watching and Favourites to it, the grid
header reads `TV 2 › Action — type to filter` (`All playlists › Action` in All) and cards lose
their playlist marks; in All, cards and category rows carry them.

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
When a film or any episode of a series has unfinished progress above zero (Continue watching),
show **Remove from Continue watching** after **Mark watched** (**Mark season watched** on a
series). It is part of the button focus walk and the page's menu (Ctrl+Enter / Shift+F10),
with Delete while the buttons have focus. Removal forgets progress without marking watched;
for a combined title it covers every copy, and for a series every episode. The toast says
"Removed from Continue watching" with Undo. Once removed, the button disappears and the
film's Resume becomes Play; Undo restores the progress and the action.
With two or more playlists of that kind (Auto, §13.0) the playlist's badge leads the meta
line — Xtream pages included.
Details load lazily with a skeleton; failures keep list-level data with a toast. Resume prompt
(overlay) when progress > `resume_min_s`: "Resume from 41:20" (focused) / "Start over".
**End of playback**: an episode shows a "Next episode in 10 s" card over the video (Enter = now,
Esc = cancel → stays on the ended episode); a movie is marked watched and the player returns
to its detail page.

### 13.7 Search overlay (`/`, Ctrl+K)
```
            ┌──────────────────────────────────────────────────────────────────┐
            │ 󰍉 bbc▏                                                            │
            │ All · Live · Movies · Series · Guide · Settings         Tab cycles│
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
A **Settings** section shows up to five matching settings, labelled `Start screen · Interface`.
Enter or a click opens Settings on that row and scrolls it into view. Channels, movies and
series come first unless the query exactly matches a setting title. Tab also offers a Settings
section with all matching settings. The settings index is built once per opening, after its
stored lists load off the UI thread; typing only matches the cached documents. A change in
parental visibility or the library invalidates it, so concealed rows cannot survive relocking.
No results: "No matches — Ctrl+K searches everything, Tab switches sections".

### 13.8 Player & OSD
```
┌──────────────────────────────────────────────────────────────────────────────────────────┐
│ 󰁍 ▣ 101  BBC One HD                                                              20:26  │ ← top OSD
│                                                                                          │
│                                        VIDEO                                             │
│                                                                    ┌ CHANNELS (c) ─────┐ │
│                                                                    │ 100 BBC Parliament │ │
│                                                                    │▸101 BBC One        │ │
│ News at Eight                                     20:00 – 21:00    │ 102 BBC Two        │ │
│ ▕██████████████████░░░░░░░░░░░░░░░▏                34 min left     └────────────────────┘ │
│ Next  21:00 Drama                                    [HD] [5.1] [CC] [⟲]                  │
│ 󰒮 󰐊 󰒭  󰗅 English (AC-3 5.1)  󰨖 Off  󰐑 Channels  󰃰 Guide  󰋚 History  󰋽 Info 󰊓 󰏌  󰕾 ━━━●─┊─ 80 % │
└──────────────────────────────────────────────────────────────────────────────────────────┘
VOD bottom: title · S02E05 · [━━━━━━━━●───────── 41:20 / 1:02:13] · −10 / +10 s · tracks
```
The OSD has two layers (UX review B1):
* **Info banner** (passive): the top and bottom bars drawn above, shown on zap, on any key and
  on mouse motion; hides after `osd_timeout_ms` (3.5 s) and never while a side panel/menu is
  open. It **does not change the key context**: ↑/↓ keep zapping, ←/→ keep recalling/seeking,
  however often they are pressed.
* **Controls focus mode**: entered only with Enter, `i` or Tab (`FocusOsdControls`) — a mouse
  click on a control just activates it, so the arrows keep zapping / seeking; pushes
  `PlayerOsd` (arrows move between controls / scrub, Enter activates); Esc
  (`LeaveOsdControls`) or Back (Alt+←, the mouse back button) leaves focus mode and the banner
  times out normally. Unbound keys fall through (Space still pauses).
* **Esc ladder in the player**: leave controls focus → hide the banner → leave window
  fullscreen → leave the player (mini-player keeps playing). Esc never *shows* the banner.
* **Which playlist**: with two or more playlists of the item's kind, the top bar shows
  `● TV` left of the clock (§13.0, §29).
* Type: title Display·s, body Title·s, badges Caption·s; `osd_accent` for progress/focus. Below
  700 pt the controls row collapses to previous · play/pause · next (live) · audio · subtitles
  · info, and the volume to its speaker (the level stays in the top bar).
* **Mouse** (every control is an OSD button: hover fill, pointing hand, tooltip with its key):
  ← at the left of the top bar leaves the player like `p`; the controls row adds Channels
  (live) and Pop out (not in the popped-out window); the row ends with the volume — the
  speaker (glyph by level; click = mute, tooltip "Mute (m)"), a slider 0–130 % with a mark
  at 100 % (click sets, drag scrubs, wheel ±5; moving it while muted unmutes; 112 pt × s wide,
  80 pt × s from 1200 pt down) and the level ("80 %", "Muted"), its room reserved before the
  controls'. The wheel over the speaker / slider steps the volume once (the video's own wheel
  handling runs after the controls, which consume the scroll). Right-click on the video opens
  the player menu (Channels…, Now / next…, History…, Last channel, favourite, aspect, zoom,
  audio…, subtitles…, stream info, jump to live, pop out / back to the full window, keep
  playing in the mini player, stop and close) — the way to every side panel in a narrow
  window; a click on the video closes a side panel or leaves focus mode. The banner stays
  while the pointer is *in use* on a bar (a button held, or moved within the last 10 s), so
  it never vanishes under a drag of the volume or the seek bar, yet a pointer parked on a bar
  (a keyboard user's, pushed out of the way) doesn't hold it for good; with the banner gone
  the pointer hides over the video (not over a control that set its own cursor). A failed /
  reconnecting stream shows Retry (and Last channel) buttons under the message; errors also
  offer **Copy error**, which copies the heading and redacted details and toasts "Copied error". The
  next-episode card has "Play now · Enter" / "Cancel · Esc" buttons and plays on a click.
  Nothing playing: a "‹ Back to {where}" button, and a click anywhere goes back.

Side panels: channel list `c` (current collection with now), mini guide `g` (now/next for the
group), history `h`/→ — each with key contexts `[List|ListFiltering, PlayerLive]`: ↑/↓ move the
panel cursor (no zap), Enter tunes, ←/Esc close (← also while the panel filters; Esc clears the
filter first), letters filter. Mouse: a ✕ in the header, the
wheel scrolls, hovering moves the cursor only with real pointer motion, one click tunes and
keeps the panel open for the next channel. Stream info `I` (codec,
resolution, fps, pixel format, HDR badge from gamma pq/hlg, audio codec/layout/rate, bitrates,
hwdec, dropped frames, cache seconds), with **Copy stream info** to copy the displayed diagnostics
and toast "Copied stream info". The panel's text lets the video's mouse handling through
(including wheel volume and double-click fullscreen); the Copy button takes its own clicks.
Digits open numeric entry (`[NumericEntry, PlayerLive]`:
digits, Enter = go now, Backspace = delete digit, Esc = cancel; auto-commit after 1.5 s).
States inside the video rect: Loading (logo + name + spinner), Buffering (%), Reconnecting
(n/max + countdown), Error (glyph + copy + "r Retry · Backspace Last channel"), Radio (logo +
now playing on black), Video unavailable (no surface). Mouse: wheel = volume (fullscreen player
only — not over the Live preview), double-click toggles window fullscreen, a single click on the
Live preview opens the player. End of playback per §13.6. Esc leaves to the previous screen;
playback continues in the mini player (bottom-right 16:9, 320 Wide / 240 Medium × rem, hidden
in Compact → status-bar chip; click = back to player, ✕ = stop; a pop-out button beside the ✕).

**While casting (§40).** `C` (or the Cast control before Fullscreen, or the menu's *Cast to a
TV…*) opens the picker, a right-edge panel in OSD colours (*This computer*, then the TVs, a
"Searching…" bar, *Add by address…*, *Firewall help…*); Enter on an unpaired TV opens the PIN
overlay first. The video area is replaced by the casting card (logo, title and now/next through
the spoiler guard, "Playing on ‹TV›", a phase line such as "Live · about 15 s behind", *Stop
casting* and *Play here*; errors add their own action). The OSD keeps its controls except audio
track, subtitles, aspect and zoom; the info panel shows the probe result, the speaker reads
"TV remote" (AirPlay volume is the TV's), and the Cast control shows the connected glyph with the
TV's name. The mini player shows the same card with a cast glyph, the compact layout a "Casting
· ‹TV›" chip whose ✕ stops, the pop-out is disabled ("Nothing to show here — playing on ‹TV›"),
and quitting asks first. Multi-view shows "Leave multi-view to cast".

**Video-only (popped-out window, §12.3).** Same screen, same keys (zap ↑/↓, volume, pause,
mute, tracks, `c`/`g`/`h` panels) plus `PopOut` (Esc / `o` = back to the full window). The
passive banner is one bottom strip — title (⏸ when paused) and volume, then the programme on
now (live) or position / duration (VOD), Body/Caption type, no scale factor; the full OSD
returns only in controls focus mode or with a side panel open. With the pointer in the window
(or paused) the strip's right side holds buttons instead of the volume and the key hint:
previous / play / next (live) or skip back / play / skip forward (VOD), the speaker (+ a
64 pt × rem volume slider from 560 pt × rem of strip; changing it flashes the level, since the
strip then has no readout), back to the full window, stop and close; below 420 pt × rem
previous / next drop out so the title and position keep room. Double-click pops back in
(instead of window fullscreen); a drag on the video moves the window (only the video-only
video senses drags: a drag-sensing video would count as hovered under every button drawn on
it). Nothing playing: "Nothing playing · o · Esc" on black, with a "Back to the full window"
button.


### 13.8.1 Delay

Local playback remembers one audio offset per output in `player.lip_sync`, a map from the
output's name to `{ offset_ms, label }`. Negative means sound earlier. `[` / `]` step by 50 ms,
clamped to ±2000 ms, apply immediately and flash “Delay: sound 150 ms earlier · MacBook Air Speakers”
(at zero, “Delay: in step”). The keys are shared by live, VOD and the pop-out; the guide's
day keys and detail's season keys stay in their own contexts. Saves wait for 500 ms without a
change, and pending values are saved on exit.

The controller uses mpv's explicit `audio-device` when set. With `auto`, it identifies the
default PipeWire/Pulse sink through `pactl --format=json get-default-sink` and `list sinks`.
Explicit PipeWire/Pulse devices also get their descriptions from the sink list; other explicit
devices use their device name. Queries run on a helper thread with a shared 750 ms timeout,
one at a time; output events within 1.5 s of the last query are ignored. A failed lookup keeps
the current output and its last good label; before any identification, `auto` uses one “This
computer” entry. The controller applies the cached output's saved `audio-delay` in seconds on
every local load and checks again every 15 s while playing or paused. Changing output names
applies that output's offset (zero when unseen) and flashes “Delay for ‹label›: ‹offset›”.
Every discovered output is remembered, including at zero; the first identification is silent.

Audio tracks (`A`) has a Delay stepper (←/→, earlier/later buttons) and Reset delay; stream info
(`I`) shows “Audio delay: ‹offset›” and the output. Settings ▸ Player ▸ Audio delay lists the
remembered outputs with the current output marked, the usual value-row stepper and a Reset row. Its keys use
the Settings model: → / l increases, Shift+← / H decreases, Enter on Reset resets; ← / h
returns to the page list. Config edits apply to every local picture in multi-view. Casting
has no audio delay controls and never receives the offset. Delay owns `audio-delay` even if
raw mpv options set it; backups keep this computer's saved outputs.

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
**Search settings** sits above the page list. Typing starts a filter from either pane;
letters, including h/j/k/l and their capitals, type instead of navigating. Backspace deletes
a character, Ctrl+Backspace a word, Ctrl+U / Esc clears and restores the original page, row
and pane. While filtering the context is `[ListFiltering, Settings]`; a setting's own focused
text editor still owns typing through `TextInput`. The mouse can focus the visible search
field. No results: `No settings match “xyz”`.

Results contain the real controls from **every visible page**, grouped under page headers.
Toggles and value arrows work in place; Enter or a row click jumps to the setting on its page
so its neighbours are visible. Action clicks run the action and text clicks open the editor
on that page. Clearing a filter keeps any changes already made. An unsaved playlist editor
still prevents leaving its page.

Each searchable row carries a page, stable target, title, description (its existing section,
notes and detail) and short aliases, derived from the same row builders as the pages. A new
row is searchable automatically. Page feature flags, per-playlist conditions and parental
concealment run before indexing; explicit category names are absent while hidden. Matching
uses the shared `textmatch::fold` case/accent folding and requires every query word to prefix
a word. Exact titles rank first, then title words, descriptions, and aliases. Page groups are
ordered by their best match. Aliases include open on / launch / startup / home; lip sync /
audio offset / out of sync; hwdec / gpu; captions / cc; menu; pin / lock / kids; spoilers;
airplay / tv / apple tv; claude / mcp / codex; export / restore; upgrade / version; look / size.
There are also short aliases for providers, guides, saved lists, notifications, automatic
rules, hidden categories, missing items, buffering, seeking, languages, pop-out and diagnostics.

Panel idiom (Enter toggles; ← steps back to the page list from every row
(§15.1 spatial panes; the source editor and the attach view stay open), → / Shift+→
increase or pick the next option, Shift+← decreases or picks the previous one; → / Enter / Tab
in the page list return to the page; focused text fields push `TextInput`, which owns
←/→/Delete/Space). A source editor with unsaved changes keeps its page: ↑/↓, Esc or a click in
the page list show "Unsaved playlist changes: Enter on Save, or Esc to discard" and return the
highlight to the editor instead of dropping them (Esc there then discards).
Reorderable rows (playlists, **EPG source priority**,
favourites lists) show a 󰇙 drag grip and move with Alt+↑/↓. Every edit goes through
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
| | Show in All playlists (§29; a view setting — saving it doesn't refresh) | `SourceSettings.in_all` | on |
| | Short code (1–3 letters / digits, unique; placeholder = the derived code; a view setting) (§13.0) | `SourceSettings.code` | from the name |
| | look-alike notice + "Give each its own colour" (§29) | `source.color` | — |
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
| | Audio delay per output (stepper · Reset; §13.8.1) | `player.lip_sync` → `{ offset_ms, label }` | {} (0 ms) |
| | Reconnect attempts | `player.reconnect_max_attempts` | 8 |
| | Resume threshold · watched % | `player.resume_min_s`, `watched_pct` | 60 · 92 |
| | Default User-Agent | `player.user_agent` | VLC/3.0.21 |
| | Advanced mpv options | `player.mpv_options` | {} |
| | Pop-out size · corner (Hyprland, §21.6) | `player.popout_size`, `popout_corner` (+ `popout_last`, remembered at pop-in) | small 480 × 270 · bottom right |
| Interface | Start screen: Home · Live TV · Guide · Last used (no playlists → onboarding; the old `"auto"` loads as Home) | `general.start_screen` | home |
| | Live TV opens on: All channels · Favourites (first open in a session, §13.2) | `interface.live_opens_on` | all-channels |
| | Sidebar · density · channel numbers · logos · quality badges · 24 h clock | `interface.*` (`quality_badges`) | auto · comfortable · on · on · on · on |
| | Playlist on cards: Auto (2+ eligible playlists, in All) · Always · Never (§13.0, §29) | `interface.playlist_on_cards` | auto |
| | The playlist Movies / Series / Live TV show (the scope bar, Alt+P; the guide follows Live TV) | `interface.playlist_scope.{movies,series,live}` | none = All playlists |
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
| Parental | enable · set/change PIN · relock after · lock groups/channels (from lists via Ctrl+L) · Explicit content: "Show explicit content" without a PIN (offers a PIN first), the categories treated as explicit (only while shown; Enter = Not explicit / Mark as explicit) | `parental.*`, overrides | off; explicit hidden |
| Backup | export (include passwords? history?) · import (merge/replace, restore settings?) | §19 | — |
| Hidden items | unhide groups/channels/VOD | overrides | — |
| Orphans | Enter = attach to…, Delete → menu with Forget | §8.3 | — |
| AI agents (§39, after Parental) | offer to connect agents after updates · keep Claude Code's plugin up to date · agents play in (Pop-out / Tuner window, §30.13) · per agent: Connect / Update / Disconnect… / Copy | `agents.offer_connect`, `agents.keep_in_sync`, `agents.play_in` (+ `agents.checked_version`) | on · on · Pop-out |
| About & logs | version, paths · Updates (§41.7): status, last check, Check now, Install now / Restart now, automatically install updates, install without asking for my password · debug logging, log viewer, export log | `updates.auto_install`, `updates.passwordless`, `general.debug_log` | on · off · off |
| | Copy app details: version and settings / data / cache / log paths; toasts "Copied app details". Follows the update controls, so Check now stays the first focusable row when available | — | — |
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
  760 px. *Added after rc3:* **type to filter**, like Omarchy's keybindings menu — the
  overlay's `Help` context is filterable, so every printable key (j/k, `/`, digits) types into
  a launcher header ("Type to filter keyboard shortcuts…") and no shortcut fires while help is
  open. A row matches when every whitespace-separated term is a case-insensitive substring of
  its keys ("ctrl+h", "f5", "alt"; arrows also answer to up/down/left/right, Esc to "escape"),
  its description or its section title ("guide" keeps the whole TV guide section). Results
  stay grouped by section in the same order (current contexts first) — the same key means
  different things per context, so a flat ranked list would lose that; empty sections are
  hidden, matches are marked with the text-selection fill, a count reads "12 of 269
  shortcuts", and no match shows "No matching shortcuts". The card's top edge freezes once
  typing starts (it shrinks downward only) and re-centres when the filter is empty again;
  typed whitespace (NBSP from AltGr+Space too) is stored as a plain space. Keys: Backspace / Ctrl+Backspace (word) / Ctrl+U edit; Esc and F1
  clear the filter, then close; `?` closes only while the filter is empty (otherwise it is
  typed); Enter closes; ↑/↓, PgUp/PgDn, Home/End scroll.
* Jobs (Ctrl+J, `[List, Modal]`): running jobs with phase + progress; Delete cancels the
  highlighted job; last 20 finished (Enter expands errors).
* Context menu (Shift+F10 / Ctrl+Enter / right-click): launcher-style, filterable, shortcuts
  right-aligned; opened by a right-click it hangs from the pointer (kept on the window).
* Confirm (destructive = urgent button, Cancel focused), PIN (`[TextInput, Modal]`), Rename
  (`[TextInput, Modal]` — Space types a space), favourites-list picker (checklist + "New
  list…"), Edit information (Ctrl+I). Backend failures in Rename and the favourites-list picker
  offer **Copy error** (the redacted message, with a "Copied error" toast); PIN prompts,
  validation messages and confirmation hints have no Copy action.
* EPG mapping (§9.4; `[EpgMapping, List]`: Enter assigns, Delete clears) and Logs (type to
  filter text; Ctrl+S view menu: minimum level, Export…) are full screens. Enter or a click
  expands a log line; **Copy line** copies its time, level, target and full redacted message,
  with a "Copied log line" toast.
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
| Locked (parental) | rows, search | lock glyph, PIN prompt on open/play; hidden from search while locked — a lock stays visible (dimmed, glyph) |
| Explicit content hidden (parental, §20.3) | everywhere | ABSENT, not dimmed: Live, guide (grid + collection picker), Movies, Series, Home rails, search, counts (group panes, scope tabs, `Library::eligible`), zap lists, favourites / History / Continue watching / Reminders, Settings lists. `--play` of it: the PIN prompt ("This channel is locked"), or without a PIN the toast "This channel is hidden by parental control (Settings ▸ Parental)" — never its name. Relock: gone in the same frame; if it plays, playback stops ("Playback stopped — parental control is on"), its pages close, open dialogs and toasts go |
| Explicit content shown | everywhere | like any content (a PIN unlock, or "Show explicit content" without a PIN); groups' menus offer "Not explicit" |
| Reconnecting / Buffering / Loading | video rect | §13.8 |
| First guide import running | rows, grid, preview | "Loading guide… 45 %" (not "No guide data") |
| Guide day without data | grid | "No guide data for Fri 17 Oct" |
| No VOD in any source | sidebar, Alt+4/5 | Movies/Series entries hidden; Alt+4/5 toast "Your playlists have no movies or series" |
| Empty favourites list | Live groups pane, Home | "Press Ctrl+F on any channel to add it here" |
| Empty history | History group | 󰋚 "Nothing watched yet" |
| No settings match | Settings | `No settings match “xyz”` (the active query); Esc clears and restores the original page, row and pane |
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
  (interleaved by score, grouped by kind), Live, Movies, Series, Guide, Settings. Locked items excluded
  until unlocked (`SearchItem.locked`); hidden items never indexed. Search results that are
  channels play fullscreen and zap through their primary group (§13.0).
* **EPG** (`search::EpgSearch`): debounced 150 ms; FTS query (§6.3) on a runtime task with its
  own reader; stale results dropped by sequence number; results show airing/upcoming first,
  then past with ⟲ when catch-up is possible.
* **Settings** (`search::SettingDocument`): the visible Settings row builders supply both
  the in-page filter and Ctrl+K. Folded title, description and alias fields are matched by
  word prefix with that ranking (§13.9); the overlay caches the small index per opening and
  rebuilds it when the session lock changes. Ctrl+K indexes settings only, excluding user-data
  rows such as reminders, hidden items and orphans. The in-page filter retains those rows
  subject to the same item and group locks as content search.
* **List filter** (`search::ListFilter`, implemented in the skeleton): per pane,
  `nucleo_matcher` scoring, ties keep list order; < 5 ms for 20k rows; leading spaces ignored;
  Ctrl+Backspace deletes a word. Channel haystacks are `"<number> <name>"`; an all-digit query
  ranks the exact channel-number match first. Poster grids filter the same way.

---

## 15. Keyboard

### 15.1 Model & layers
* **Contexts** (`keymap::KeyContext`): Global, List, ListFiltering, PosterGrid,
  PosterGridFiltering, Detail, Guide, PlayerLive, PlayerVod, PlayerOsd, PopOut, NumericEntry,
  EpgMapping, Search, Modal, TextInput, Panel, Checklist, Help.
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
  * Filterable contexts (List, ListFiltering, PosterGrid, PosterGridFiltering, Search, Panel, Help):
    an unbound printable char becomes `FilterInput(c)` — **launcher idiom: letters type in lists
    and grids, so no j/k there**; Settings also types to filter. Vim keys remain in the guide
    and player.
  * **TextInput owns the keyboard**: after its own table only Ctrl/Alt chords and F-keys may
    reach `Global`; Space, ←/→, Delete, Backspace… return `None` and go to the text widget;
    contexts below it are never consulted (typing a space in Rename can't confirm the dialog).
  * Modal swallows printable text it doesn't bind; PlayerOsd, PopOut, NumericEntry and
    EpgMapping fall through to the context below.
  * Key auto-repeat never produces `Parent`/`Back`, a filtered list's `Left` or `LastChannel`
    (holding Backspace to clear a filter stops at the empty filter; holding ← steps out of a
    filtered list once, and the ← that closed a player side panel never zaps).
* **Spatial panes** (Omarchy style: arrows move the highlight between side-by-side panes): ← at
  the left edge of the focused pane steps to the pane on its left wherever one exists; → (or
  Enter) opens / moves right; Tab, Shift+Tab and Esc are unchanged; the mouse is unaffected.
  * Lists: ← is `Parent` in `List`; in `ListFiltering` it is `Left`, which `ListState` turns
    into `Parent` whatever the filter (the filter is kept for when the pane is focused again).
    Backspace stays `Parent` only on an empty filter (one frame can carry "a" + Backspace).
    Live: channels → groups (the drawer at Medium, the groups pane at Compact); EPG mapping:
    candidates → channels; menus and pickers close; a player side panel closes.
  * Grids: ←/→ move between posters; ← on the first column is `Parent` → the categories
    (drawer at Medium) — also while filtering (the filter is kept) and on an empty grid.
  * Settings: ← is the page list from every content row; values change with → and
    Shift+←/→ (Interface is almost all value rows, so "← changes a value where there
    is one" would almost never reach the page list). The page list has nothing on its left.
  * ←/→ keep their own meaning where there is no pane to the left or they move within the
    pane: the guide (← is the previous programme, into the past for catch-up; the channel
    column never takes focus), Home rails, detail pages, the player (seek / zap / OSD), text
    fields (cursor), onboarding and Edit information (values), Search (no cursor; sections
    are Tab filters). The app sidebar is never reached with ← (Alt+digits switch screens).
  * `Global` contains only Ctrl/Alt/F-key chords so it never steals typing.
  * **No screen reads raw keys**: every key is in a table, so help and hints are complete.
* **Esc ladder**: clear filter → close overlay/side panel → leave OSD controls focus → hide the
  info banner → leave window fullscreen → leave the player (mini player keeps playing) → back
  in the route stack. Esc never shows the banner. Alt+← = Back. The mouse back button IS Esc:
  the app resolves a synthetic Esc against the same context stack, so it walks this whole
  ladder (clears a filter, leaves the OSD controls or a text field, closes an overlay, pops the
  video-only player back in) rather than skipping to the route stack. The forward button is
  swallowed and does nothing (there is no forward history).
  *Decided in phase 3:* Back on a lone sub-page (nothing beneath it in the route stack, e.g. a
  restored last screen) goes to its parent screen: movie / series details → Movies / Series,
  Logs and EPG mapping → Settings, the player → Live TV.
* *Decided in phase 3:* Ctrl+P (open the player) acts only while something is playing (any
  non-Idle player state); with nothing playing it does nothing rather than open an empty player.
* **Label selection**: labels are not selectable app-wide, so a label inside a clickable row
  leaves the click to that row. Standalone notes and errors with no click target beneath them
  may opt into selection (Settings notes and onboarding failures). Use explicit Copy controls
  for diagnostics and paths in interactive panels; each copy gives a "Copied …" toast.
* **Mouse parity**: everything is clickable; hover moves list cursors only after real pointer
  motion (Omarchy `pointerGate`) and never acts; **one click = exactly what Enter does in that
  context** on every selectable item (list rows of every kind, poster cards, Home tiles,
  episode rows, copies, guide cells, Up next rows, Logs lines, EPG mapping rows); right-click =
  context menu; wheel = volume only in the fullscreen player; double-click has a meaning of
  its own only on video: window fullscreen (in the video-only player: pop back in). Widgets with
  their own single click keep it (buttons, scope tabs, the "{category} ▾" button, a stack's
  copy-count badge). *Decided in the single-click pass:* a click on an EPG mapping channel
  moves on to its guide channels as Enter does (accepting the proposal is Shift+Enter or the
  Accept button, no longer a double-click); a click on a Recent changes row undoes it as Enter
  does (the detail's "Undo this" button stays); a guide cell click opens the popover (a click no
  longer only focuses; the preview-rest timer starts from keys). *Decided in the mouse pass:*
  * **Back affordance**: a page or pane that Esc leaves shows a borderless `‹ {where}` button
    (`widgets::button::back_button`, accessible name "Back to {where}"): detail pages, Logs,
    EPG mapping and Settings when something is below them (`UiCtx::back`: the route below, or
    a lone sub-page's parent — the same target as Esc), and the one-pane-at-a-time views of
    Compact (Settings page → page list, EPG candidates → channels, Live groups → channels).
    A click on the lit sidebar tab of a sub-page (Movies on a movie's page, Settings on Logs)
    goes back down to that tab where it is on the stack instead of opening it afresh.
  * **Overlays**: a ✕ "Close (Esc)" in the card header (Help, Jobs, Search, Resume, the
    tracks panel); a click or right-click outside closes the overlay — except while it holds
    an unsaved edit (`Overlay::is_dirty`: a typed rename, a new list's name, Edit information,
    changed category ticks), when the card border flashes instead (and the pointer's tooltip
    says "Unsaved changes — Cancel or Esc to discard") and Esc / Cancel close.
  * **Right-click** opens the same menu as the menu key, at the pointer (lists, grids, Home
    tiles, guide cells and channels, episode rows, the detail pages' header, search hits).
    A menu opened by an overlay (search's item menu: "Play now", then the item shortcuts)
    answers that overlay, never the screen below. Detail pages also have a `⋯` button.
  * A back button runs exactly what Esc runs there (Settings' sub-views leave a field being
    edited first, keeping its text). A pushed TV guide (Ctrl+G from Home / Search) has one
    too. Long labels are cut to 24 characters on screen ("‹ |UK| SKY CINEMA PREMIE…"); the
    accessible name keeps the whole label.
  * **Double-click safety**: a double-click is one activation, in two layers.
    (1) Widgets act on `widgets::fresh_click` — a click that isn't the 2nd/3rd of a multi-click
    (egui reports `clicked()` on every release) — so a row isn't played twice and a tick or
    toggle isn't flipped back. (2) When a click changes what is under the pointer, the app
    (`ui::click_guard`, in `raw_input_hook`) drops the next primary press and its release that
    come within the double-click delay (+50 ms, extended by each dropped press) and 6 px of that
    click. The app sets egui's double-click delay to 0.5 s (`click_guard::DOUBLE_CLICK_DELAY`;
    egui's 0.3 s is shorter than desktops count, GTK 0.4 s), and both layers read it, so a
    normal-speed double-click is one click. The drop happens automatically when the route or
    overlay stack changed in that frame or the next (a title's page, the player, a popover or menu opening; a menu or overlay closing), and when a
    screen calls `widgets::swapped_under_pointer` for a swap inside one route (a Movies / Series
    category or a Live group opening below Wide, the Compact Settings page list, EPG mapping's
    one-pane Compact and a candidate assigned, onboarding's choices). So a double-click on a
    channel never also toggles fullscreen on the player it opened, a double-click on a menu item
    never activates the row under the menu, and a category's second click never opens a title.
    Drag ends never click: egui reports no click past 6 px or 0.8 s, and the reorder grips
    (Settings, Up next) are drag-only widgets on top of their rows.
  * **The focusing click**: egui doesn't say whether a click also focused the window. With
    Hyprland's default `follow_mouse = 1` the pointer focuses the window before any click, so
    the click hits the item hover already lit — what was meant. For click-to-focus setups the
    first click is ambiguous, so `click_guard` drops the primary press (and its release) that
    the focus comes with: a press into the unfocused window is held back until its release
    and dropped if `WindowFocused(true)` arrives meanwhile, as is a press within 100 ms after
    it with no pointer motion in between (hover focus always comes with motion, so a quick
    click right after the pointer crosses into the window under `follow_mouse = 1` acts). A
    held press that brought no focus (`follow_mouse = 3`, a window that never takes the
    keyboard) is delivered at its release as the click it was. A held press that turns into a
    drag (moves > 6 px) is delivered at once, so scrollbars, sliders, the reorder grips,
    multi-view swaps and the pop-out's window move work in an unfocused window too; one held
    still for > 0.8 s does nothing (it isn't a click). Pointer motion always passes, so hover
    keeps pointing. The developer screenshot
    driver's injected clicks come after the guard and are never dropped.
  * Clickable widgets show the pointing hand; Settings pages light on hover while the keyboard
    cursor is in the page's content (never a second cursor); the onboarding choices move the
    focus with real pointer motion, like menus; below Wide one click on a Movies / Series
    category opens it (the drawer closes);
    the Live groups drawer has a close button and closes on a click beside it; group panes'
    bulk-hide footer has `[Hide N]` / `[Clear]`; the Jobs panel has a cancel button per running
    task; toasts show a ✕ on hover.
* **Discoverability**: `?`/F1 help overlay (current contexts first) and the status-bar hint line
  (`keymap::hint_line`: curated verbs, keys looked up in the tables).
* No Super bindings inside the app (Omarchy owns Super). `Ctrl+Space` is avoided (fcitx5 IME
  toggle on Omarchy). Media keys arrive via MPRIS.

### 15.2 Tables (generated: `cargo run --example keymap_md`)

Casting (§40) adds `C` (*Cast to a TV…*, `Action::CastMenu`, in the player's common table) and
`Action::StopCasting` (no key: the menu, the card and the chip; the picker's *This computer* is
Play here) and `Action::PlayHere` (no key: the alert after the TV stopped a cast). *Cast to…* on
the focused item (`Action::CastItem`, cast design §3.1) is `C` in the guide and on detail pages,
which don't filter, and Alt+C in `ITEM_ACTIONS` (lists, grids, Home, detail pages), Search and
the guide: lists and grids filter on every printable key, so a plain `C` there would steal a
filter's capital C. Alt+T (`Action::PlayOnMenu`, global) opens the Play on chooser (§3.9).
`Action::CastUnlocked` is the PIN prompt's answer for a cast item, never bound. All are gated
by `keymap::feature_on` on `cast::ENABLED`, so the generated rows list them once casting is
switched on. The pickers and the PIN overlay reuse `Modal` / `List`; there is no new context and
no Ctrl+Shift+C (the terminal's copy).

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
| `Alt+6` | Up next |
| `Alt+7` | Watchlists |
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
| `Ctrl+B` | Show / hide spoilers of the focused item |
| `Ctrl+Shift+B` | Spoiler guard… (pause, I'll watch it later, guard a topic) |
| `Ctrl+Shift+T` | Sleep timer… |
| `Alt+P` | Choose the playlist shown (Movies, Series, Live TV, guide) |
| `Alt+T` | Play on… (this computer or a TV) |
| `Ctrl+PgDn` / `Ctrl+PgUp` | Next / previous playlist (Movies, Series, Live TV, guide) |

#### Watchlists (`Watchlists`)

| Keys | Action |
|---|---|
| `→` | Open entries / move right |
| `Ctrl+I` | Episodes… / film details |
| `Ctrl+W` | Mark current episode / title watched; done entry unwatched |
| `Ctrl+Shift+Enter` | Play just this one |
| `s` | Skip in this list / unskip |
| `o` | In order / Any order |
| `+` | New list… |

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
| `Space` / `Insert` | Mark (groups: Ctrl+H then hides all marked) |
| `Ctrl+O` | Play from another playlist (Also on)… |
| `Ctrl+F` | Save to / remove from My list (channels: Favourites) |
| `Ctrl+Shift+F` | Watchlists… (channels: Favourites lists…) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign guide channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Ctrl+N` | Add to Up next |
| `Ctrl+Shift+N` | Play next (Up next) |
| `Ctrl+M` | Watch alongside |
| `Alt+C` | Cast to a TV… |
| `Shift+F10` / `Ctrl+Enter` | Context menu |
| `a–z, 0–9 …` | Type to filter |

#### Lists while filtering (`ListFiltering`)

| Keys | Action |
|---|---|
| `↑` / `↓` | Move |
| `PgUp` / `PgDn` | Page up / down |
| `Enter` | Play here / open (playing → fullscreen) |
| `Shift+Enter` | Play fullscreen |
| `←` | Back to the previous pane |
| `Backspace` | Delete character |
| `Ctrl+Backspace` | Delete word |
| `Ctrl+U` / `Esc` | Clear filter |
| `Tab` / `Shift+Tab` | Next / previous pane |
| `Insert` | Mark (groups: Ctrl+H then hides all marked) |
| `Ctrl+Shift+H` | Hide the groups with the words typed |
| `Ctrl+O` | Play from another playlist (Also on)… |
| `Ctrl+F` | Save to / remove from My list (channels: Favourites) |
| `Ctrl+Shift+F` | Watchlists… (channels: Favourites lists…) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign guide channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Ctrl+N` | Add to Up next |
| `Ctrl+Shift+N` | Play next (Up next) |
| `Ctrl+M` | Watch alongside |
| `Alt+C` | Cast to a TV… |
| `Shift+F10` / `Ctrl+Enter` | Context menu |
| `a–z, 0–9 …` | Type to filter |

#### Tick lists (`Checklist`)

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
| `Space` | Show each copy / fold them (stack card) |
| `Ctrl+O` | Play from another copy… |
| `Ctrl+F` | Save to / remove from My list (channels: Favourites) |
| `Ctrl+Shift+F` | Watchlists… (channels: Favourites lists…) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign guide channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Ctrl+N` | Add to Up next |
| `Ctrl+Shift+N` | Play next (Up next) |
| `Ctrl+M` | Watch alongside |
| `Alt+C` | Cast to a TV… |
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
| `Ctrl+O` | Play from another copy… |
| `Ctrl+F` | Save to / remove from My list (channels: Favourites) |
| `Ctrl+Shift+F` | Watchlists… (channels: Favourites lists…) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign guide channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Ctrl+N` | Add to Up next |
| `Ctrl+Shift+N` | Play next (Up next) |
| `Ctrl+M` | Watch alongside |
| `Alt+C` | Cast to a TV… |
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
| `Ctrl+O` | Play from another copy… |
| `C` / `Alt+C` | Cast to a TV… |
| `Ctrl+F` | Save to / remove from My list (channels: Favourites) |
| `Ctrl+Shift+F` | Watchlists… (channels: Favourites lists…) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign guide channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Ctrl+N` | Add to Up next |
| `Ctrl+Shift+N` | Play next (Up next) |
| `Ctrl+M` | Watch alongside |
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
| `R` | Follow… (remind me every time it's on) |
| `Ctrl+W` | Mark seen (lifts the spoiler guard) |
| `Ctrl+N` | Add to Up next |
| `Ctrl+Shift+N` | Play next (Up next) |
| `Ctrl+M` | Watch alongside |
| `C` / `Alt+C` | Cast to a TV… |
| `f` | Collection: favourites lists, groups, all |
| `0` / `1` / `2` / `3` / `4` / `5` / `6` / `7` / `8` / `9` | Jump to channel number |
| `Ctrl+F` | Add channel to Favourites |
| `Ctrl+Shift+F` | Favourites lists… |
| `Ctrl+E` | Assign guide channel |
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
| `[` / `]` | Delay: sound earlier / later (50 ms) |
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
| `C` | Cast to a TV… |
| `u` | Up next |
| `n` / `N` | Next / previous in Up next |
| `Ctrl+M` | Watch alongside… (choose the channel) |
| `Ctrl+F` | Save to / remove from My list (channels: Favourites) |
| `Ctrl+Shift+F` | Watchlists… (channels: Favourites lists…) |
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
| `Ctrl+O` | Play from another copy… |
| `Space` | Play / pause |
| `m` | Mute |
| `+` / `=` / `-` | Volume up / down |
| `Enter` / `i` / `Tab` | On-screen controls |
| `I` | Stream info (codec, resolution, bitrate) |
| `a` | Next audio track |
| `A` | Audio tracks… |
| `s` | Next subtitle track |
| `S` | Subtitles… |
| `[` / `]` | Delay: sound earlier / later (50 ms) |
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
| `C` | Cast to a TV… |
| `u` | Up next |
| `n` / `N` | Next / previous in Up next |
| `Ctrl+M` | Watch alongside… (choose the channel) |
| `Ctrl+F` | Save to / remove from My list (channels: Favourites) |
| `Ctrl+Shift+F` | Watchlists… (channels: Favourites lists…) |
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

#### Player — multi-view (`MultiView`)

| Keys | Action |
|---|---|
| `w` | Next picture |
| `Ctrl+←` / `Ctrl+→` / `Ctrl+↑` / `Ctrl+↓` | Picture in that direction |
| `M` | Sound from this picture |
| `W` | Swap the pictures' places |
| `t` | Layout: side by side, big + small, picture in picture |
| `q` | Close this picture (the other carries on) |
| `Ctrl+M` | Watch alongside… (choose the channel) |

#### Pop-out player (`PopOut`)

| Keys | Action |
|---|---|
| `Esc` / `o` | Back to the full window (also double-click) |
| `f` | Back to the full window, fullscreen |
| `Delete` | Cancel the coming switch / end the news flash (back to what you were watching) |

#### Channel number entry (`NumericEntry`)

| Keys | Action |
|---|---|
| `0` / `1` / `2` / `3` / `4` / `5` / `6` / `7` / `8` / `9` | Type the channel number |
| `Enter` | Go now (otherwise after 1.5 s) |
| `Backspace` | Delete digit |
| `Esc` | Cancel |

#### Guide fixes (`EpgMapping`)

| Keys | Action |
|---|---|
| `Enter` | Issues: show its guide channels · Guide channels: assign |
| `Delete` | Reset to automatic (guide channel and time shift) |
| `Shift+Enter` | Accept the proposed fix (every marked channel) |
| `Ctrl+A` | Mark all shown / none |
| `Ctrl+Z` | Undo the last guide fix |
| `Ctrl+Shift+→` / `Ctrl+Shift+←` | Time shift +1 h / −1 h |

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
| `Ctrl+F` | Save to / remove from My list (channels: Favourites) |
| `Ctrl+Shift+F` | Watchlists… (channels: Favourites lists…) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign guide channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Ctrl+N` | Add to Up next |
| `Ctrl+Shift+N` | Play next (Up next) |
| `Ctrl+M` | Watch alongside |
| `Alt+C` | Cast to a TV… |
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

#### Panel (`Panel`)

| Keys | Action |
|---|---|
| `↑` / `k` / `↓` / `j` | Move |
| `←` / `h` | Back to the page list |
| `→` / `l` / `Shift+→` / `L` | Increase / next option |
| `Shift+←` / `H` | Decrease / previous option |
| `Alt+↑` / `K` / `Alt+↓` / `J` | Move up / down (reorder: guide priority, playlists, lists, Following rules) |
| `Tab` / `Shift+Tab` | Next / previous section |
| `Enter` / `Space` | Toggle / open |
| `x` / `Delete` | Delete / more actions |
| `Esc` | Back |
| `?` | Keyboard help |

#### Settings (`Settings`)

| Keys | Action |
|---|---|
| `↑` / `↓` | Move |
| `←` | Back to the page list |
| `→` / `Shift+→` | Increase / next option |
| `Shift+←` | Decrease / previous option |
| `Alt+↑` / `Alt+↓` | Move up / down (reorder: guide priority, playlists, lists, Following rules) |
| `Tab` / `Shift+Tab` | Next / previous section |
| `Enter` | Toggle / open |
| `Space` | Toggle / open (types a space while filtering) |
| `Delete` | Delete / more actions |
| `Esc` | Back |
| `Backspace` | Delete filter character |
| `Ctrl+Backspace` | Delete filter word |
| `Ctrl+U` | Clear filter |
| `a–z, 0–9 …` | Type to filter |

#### Keyboard help (`Help`)

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
  (memory-maps each packaged font file under `/usr` once for the process; egui borrows the
  mapped bytes through `FontData::from_static`, so the ~25 MB of fallbacks are page cache, not
  two heap copies; a font elsewhere, e.g. `~/.local/share/fonts`, which a `cp` can overwrite in
  place under a live mapping, is read once and kept) run off the UI thread — at
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

* **Settings change log**: every applied config edit emits INFO in the existing log, e.g.
  `settings: start screen Home → Live TV (key →)`. The queued edit carries its input: key,
  click, agent or restore (automatic housekeeping uses system / exit). Text fields and
  dynamic maps say `changed`; log labels come from the fixed schema, never playlist names,
  URLs, credentials, device names or programme titles. Enum-like strings are allowlisted.
  Per-output corrections say `audio delay for the current output`, with the old/new ms,
  once after 500 ms without a change to that output (also flushed on exit). Other config
  settings batch independently the same way, keeping the first old and last new values,
  never one line per key repeat.
  Restoring a backup logs each changed preference with `restore`. Agent multi-view layout
  edits use `agent`; asynchronous update-switch edits retain the initiating key/click.
  Playlist editor saves and guide enable/offset changes also log without identifying the
  playlist or guide. No-op config edits emit nothing.

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
* **Casting** (§40; cast design §9.3 and §9.4): the first LAN-facing listener and the first root
  helper. The listener binds only the address the kernel uses to reach the TV (never `0.0.0.0`),
  on `cast.media_port`, only while a session exists; it answers `GET`/`HEAD` of `/c/<token>/<file>`
  with a fixed file-name pattern from the TV's own IP and otherwise one identical 404, with no
  redirects and no listing. Tokens are 128-bit, constant-time compared, one per load generation,
  `Secret`s registered with `redact`. The TV only ever sees `http://<lan-ip>:<port>/c/<token>/…`:
  no provider host, path or login, and no playlist rewritten with absolute URLs. ffmpeg and
  ffprobe get only a loopback token URL on their command line (a test asserts it); the loopback
  token is visible to local users in `ps` for the session (accepted: a single-user desktop; the
  proxy accepts only 127.0.0.1, one client). Device-supplied strings (names, models, versions,
  plists) are data: capped, sanitized, bplist depth and size limited. Pairing keys sit in the
  0600 database, out of backups; the PIN is used once and never logged or registered as a secret.
  Toasts, notifications and agent replies carry typed `CastErrorKind` sentences only. **The root
  helper** `/usr/lib/tuner/tuner-firewall` (std only) is started through `pkexec
  --disable-internal-agent` under the polkit action `io.github.rthoms334.omatuner.firewall`
  (`auth_admin_keep`); it validates a verb, an interface present in `/sys/class/net`, an RFC 1918
  IPv4 network (/8–/32, zero host bits) and a port 1024–65535, refuses to run unless root with
  `PKEXEC_UID` set, and runs one fixed `/usr/bin/ufw` command; the arguments are not secrets and
  polkit's agent, not Tuner, takes the password. No agent tool runs `pkexec` or `sudo`. The
  residual risk is a LAN host that both spoofs the TV's address and has the token.
* **Never on a command line**: mpv is embedded (no `ps` exposure); `force-media-title` keeps
  URLs out of mpv's title; MPRIS exposes titles and logo URLs only (http(s) logos are fine).

### 20.3 Parental PIN
4–8 digits, stored as hex(blake3(salt ‖ pin)) with a random salt in config; locked groups/
channels/VOD require the PIN to open/play; unlock lasts until restart or `relock_min`; three
wrong attempts → 30 s cooldown; locked items hidden from search while locked. It's a speed bump
for children, not security (documented in Settings).

**Locked ≠ explicit (user decision).** A *lock* is the user's own: the item stays visible —
dimmed, lock glyph — and asks for the PIN on open/play. *Explicit* (adult) content is hidden
behind parental control: absent, as if the playlist didn't have it.

* **Detection** (`sources::explicit`, at import; stored, §6.2.3). A category is explicit when
  the provider flags it (Xtream `is_adult` on `get_*_categories`, read leniently: `"1"`, `1`,
  `true`; `"0"`, `0`, missing = no) or its raw provider name has one of the words `XXX`, `18+`
  / `+18` (also `18 +`), `Porn` / `Porno`, `Erotic` / `Erotica` / `Erotik` / `Érotique` /
  `Erotico`, `Adult` / `Adults` / `Adulto(s)` / `Adulte(s)` / `Adulti` / `Erwachsene`,
  Cyrillic `ЭРОТИКА` / `ПОРНО` / `Взрослые` / `Взрослых` — whole words through the shared
  tokenizer (`name_tokens`: `|XXX|`, `★ 18+ ★`, `ᴬᴰᵁᴸᵀ`, `XXX-Movies`, `18+XXX`, Cyrillic
  `ХХХ`; circled, squared, fullwidth and mathematical letters `ⓍⓍⓍ` / `𝐗𝐗𝐗` read as ASCII), except
  `Young Adult(s)`, `New Adult`, `Adult Swim`, `Adult Animation` / `Animated` / `Cartoon(s)` /
  `Education` / `Learning` / `Contemporary`. Not `Essex`, `XXXL`, `U18`, `Adultery`. Item
  names are never classified. An **item** is explicit when one of its categories is, or the
  provider flags the item itself (`is_adult` on live / VOD / series streams) and none of its
  categories is marked "Not explicit".
* **The user's word** per category (`group_override.explicit`): "Mark as explicit" / "Not
  explicit" in the group / category menus (Live, Movies, Series) and Settings ▸ Parental's
  list; it beats the provider and the names and survives refreshes. The word is always
  stored (never "back to as detected": the detection may have changed since, and the toast
  must hold). While hidden, the toast doesn't name the category.
* **When it is hidden** — `ParentalConfig::hides_explicit(unlocked)`: with a PIN (parental lock
  on), while the session is locked; without one, unless Settings ▸ Parental ▸ **Show explicit
  content** is on (off by default; turning it on offers "Set a PIN first (recommended)" or
  "Show without a PIN"; removing the PIN turns it off again).
* **One predicate.** `store::library::Visibility` is the library build's only visibility rule:
  `load_visible` leaves explicit groups and items out of the snapshot entirely (`channels`,
  `groups`, VOD vectors, `members`, `per_source` counts, `recent_channels`, and
  `SourceMeta::has_*`, so a playlist with only explicit movies isn't `eligible`). Everything
  built from the snapshot — lists, collections and zap lists, numeric entry, search, Home,
  favourites / History lookups (`by_ref`) — follows without asking. `Library::explicit`
  (`ExplicitIndex`) records what is explicit either way — disabled playlists' items too —
  and `Library::conceals(&ItemRef)` is the check for rows read from the database (Continue
  watching, Reminders, Settings ▸ Hidden / Favourites counts); it fails closed before the
  first build. Group counts (`item_count`, so hide-empty too) leave explicit members out, and
  the "Choose what to show" totals leave explicit categories out. Settings ▸ Orphans leaves
  out orphans whose category (`fingerprint.norm_group`: the stored category and the user's
  word on it, or its name) is explicit, with a count. P2's merged stacks ask
  `Visibility::admits` per copy.
* **Switching.** The app keeps an `ExplicitGate` (shared with the jobs and the reminder
  scheduler) in step with the parental state every frame (`TunerApp::sync_explicit`; the
  relock timer repaints on time). Unlock → a `RebuildLibrary` that shows it (arrives < 0.4 s;
  nothing else changes, no flicker). Relock → `Library::hiding_explicit` re-filters the
  snapshot on the UI thread in the same frame (a copy of the indexes; no rebuild to wait for),
  a build that started before is filtered on arrival, explicit playback stops, its detail pages
  and groups close, open dialogs and toasts go.
* **Authoritative elsewhere.** The player controller never zaps or recalls onto an explicit
  channel while hidden (`MediaResolver::is_explicit`, any sender — MPRIS always counts as
  locked); `--play` / a reminder's Watch ask for the PIN (or toast "hidden by parental
  control") without naming it; reminders on explicit channels stay silent while hidden
  (pending, announced if still airing once shown); detail-load jobs never name an explicit
  title (jobs overlay, log lines), and no INFO+ log line carries one (mpv's own lines are
  requested at `warn`, so its `info` stream tags never reach the log).
* **Locked rows are dimmed** while the session is locked: the app records the session state
  each frame (`widgets::set_session_locked`), and channel / group rows paint locked entries in
  `text_disabled` and posters under a scrim; they paint normally once the PIN opens it.
* **Not covered:** a backup export is a full dump — favourites, progress and reminders of
  explicit titles included, whatever the lock. Keep backup files private.

### 20.4 The update helper (§41): threat model
`/usr/lib/tuner/tuner-update` is the one program of the updates that runs as root, started
only through `pkexec --disable-internal-agent` under the actions of
`packaging/polkit/io.github.rthoms334.omatuner.update.policy`: `install` (auth_admin_keep) and
`passwordless` (auth_admin). It must be safe to allow *without a password* (the user's switch),
so it trusts nothing it is given: not its argument, not the files, not the environment.

* **The trust root is the release key.** A package installs only when its bytes verify against
  the minisign public key compiled into the helper (`packaging/update-key.pub` via
  `include_str!`, pre-hashed mode, the key id and the trusted comment's global signature
  included), its file name says a stable version (`<n>.<n>.<n>`: pre-releases are signed with
  the same key and never installed), and pacman's own reading of the verified copy says name
  `omatuner`, this machine's architecture, the file name's version, newer than the installed one
  (`vercmp`, checked against pacman's `vercmp` on random input). The trusted comment isn't read:
  the signature covers the bytes, so the `.PKGINFO` pacman reads. A key file that is the
  placeholder (or doesn't parse) installs nothing (exit 6).
* **Network attacker, mirror, a GitHub account without the key:** any bytes they serve fail the
  signature. They can withhold updates, or serve an *older* signed release: the same version or
  a downgrade is refused; a genuine release between the installed one and the newest can be
  installed (the next check moves on). The app verifies too, but only to refuse early: the
  helper's check is the one that counts.
* **Local malware as the user** (what the passwordless rule exposes): it can run `install`
  without a password, and so install a genuine, signed, newer stable Tuner earlier than the user
  would — nothing else. It can't choose the package (name, signature, version and arch are
  checked), pacman's arguments, a downgrade, or turn the rule on (`passwordless` always asks).
  With the rule off it can only pop the password dialog, which names Tuner's update.
* **The caller's process isn't trusted either.** What pkexec starts keeps the caller's resource
  limits (pam_limits only sets what limits.conf names) and cgroup — under Omarchy a scope of
  the user's systemd, whose `cgroup.kill`, `cgroup.freeze` and `memory.max` the user owns. Run
  there, pacman could be killed or frozen halfway (`RLIMIT_FSIZE`/`RLIMIT_CPU`, cgroup.kill):
  a half-installed Tuner and a stale `db.lck`; and a frozen helper stretches any window between
  a check and its use. Two helpers at once shared root's copy: with one frozen after `pacman
  -Qip` read its copy, another (killed once it had staged) replaced that copy with an older
  genuine package named like the newer one (its signature verifies: it covers the bytes, not the
  name), and the first's `pacman -U` downgraded. So
  `install` only checks who asked and the argument's shape and runs `install-as <uid> <path>` as
  the transient system service `omatuner-update.service` (`systemd-run --wait --pipe --collect
  --expand-environment=no`; the path free of control characters): PID 1's child, in
  system.slice with the system's limits, nothing of the caller's; every check below happens
  there. The fixed unit name makes installs one at a time (systemd refuses a second while the
  first is loaded: exit 5), so root's copy is one run's; the service empties its folder first.
  `install-as` is refused through pkexec (`PKEXEC_UID` set). The service outlives the caller:
  once started, an install finishes.
* **Other users:** the rule names the user (`subject.user`, from `getent passwd PKEXEC_UID`,
  never an argument; a name outside `[A-Za-z0-9._-]` with an optional final `$` is refused) and
  needs `subject.local && subject.active`; the downloads folder is 0700.
* **The path (TOCTOU):** absolute, plain components, each folder opened from the one before with
  `O_NOFOLLOW|O_DIRECTORY` (no symlink anywhere, `/proc/<pid>/root` included), owned by root or
  the user and not writable by others unless sticky; the last folder the user's and 0700; the
  file and its `.minisig` opened with `O_NOFOLLOW|O_NONBLOCK|O_NOCTTY`, then checked on the open
  descriptor: a regular file of the user's (no FIFO or device), one link, ≤ 256 MiB (≤ 4 KiB).
  The bytes are read from that descriptor and verified in memory, then written to a new file in
  root's private `/var/cache/omatuner/` (0700 root, refused if it is anything else); pacman
  reads only that copy, which is removed after. Nothing a user can touch is read twice.
* **Environment and argv:** pkexec cleans the environment; the helper still runs pacman and
  getent with `env_clear` (`PATH`, `LC_ALL=C`), no shell, stdin closed, fixed argv (`-Qip`,
  `-Q omatuner`, `-U --noconfirm --noprogressbar -- <copy>`). Exit codes: 0 done, 2 bad
  arguments / not root through pkexec, 3 refused, 4 pacman, the service or the rule file failed,
  5 busy (pacman's lock, or another install's service), 6 no release key; never 126/127. The
  service's codes and message are passed on by `install`.
* **What a signed release can do:** anything a package can (root scriptlets, files): whoever holds
  the secret key and its password can update every Tuner. The key lives in the repository
  secrets and the maintainer's offline backup (README "Release signing"). A leaked key needs a
  new key shipped by hand (a Tuner trusts only its built-in key).
* **Not covered:** pacman's partial-upgrade risk (`-U` installs against the system as it is; the
  release's dependencies are declared, its libraries are built on current Arch);
  `LocalFileSigLevel = Required` makes pacman refuse the copy (exit 4, the message says so);
  pacman's hooks and the package's scriptlet run as on any install.

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

**Cast alerts** (§40, cast design §3.8): a failure or a stop on the TV that needs the user, while
Tuner's window is not on a visible workspace (not focused and not shown by
`platform::popout::self_shown`; off Hyprland an unfocused window counts as not seen, so an alert
is never missed), goes through `platform::notify::notify_cast` with a fixed sentence (no device
or provider text) and the action **Show**, which sends `PlatformEvent::ShowPlayer`: Tuner is
raised (`hypr::focus_self`) onto the player, where the casting card has the buttons (Allow on
firewall, Try again, Play here). With no notification server the in-app toast is used. Phase
changes never alert. A switch reminder that fires while casting retunes the TV and does not raise
or pop the window (§30.13). "Up next is waiting for your PIN" is sent when the window is not
focused, since no local video is suspended while casting.

### 21.4 Single instance & CLI
`$XDG_RUNTIME_DIR/tuner/tuner.sock`; a second `tuner [--view guide] [--play …] [file/url]`
hands its request over and exits 0 (stale socket removed). Works with `omarchy-launch-or-focus
'^tuner(-popout)?$'` (anchored: the pattern also matches window titles; `-popout`: a window
started straight into the pop-out, §21.6) and the omarchy-menu entries. Request handling (`TunerApp::handle_ipc`, also for the
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
* **Pop-out**: (unless already) float → size → move → (unless already) pin → (only if it was
  already floating) raise → tag `pop`
  (Omarchy rounds `tag:pop` windows). Size: Settings ▸ Player ▸ Pop-out size, Small 480 × 270
  (default) or Medium 640 × 360; place: the chosen corner (default bottom-right) of the
  window's monitor, inside its reserved area (bar), 10 px margin (Omarchy's `gaps_out`, so it
  lines up with tiled windows). A refused step undoes the steps before it (no half popped-out
  window, no record). A maximized window (Hyprland's fullscreen mode 1) is un-maximized first
  (`hl.dsp.window.fullscreen({ window, mode = "maximized", action = "unset" })`; classic
  syntax has no per-window form, so there the toast asks to un-maximize). The app leaves
  window fullscreen first, before a pop-out or a pop-in (a fullscreen window can be neither
  pinned nor unpinned); the worker retries for 1 s until Hyprland sees it (and, popping out,
  for up to 3 s until a just-created window is mapped).
* **Focus**: no pop-out step takes the keyboard (checked against Hyprland 0.56's
  `Actions::floatWindow` / `pinWindow` / `resize` / `move` / `tag`), and an IPC `--play …
  --popout` or `--reminder` never calls `focus_self`; nor does an agent's play that opens in
  the pop-out (§30.13). `alter_zorder` is the exception: it
  simulates a pointer motion after raising, and with `follow_mouse = 1` a pointer that moved
  in the last 0.5 s over the raised window hands it the keyboard. So the raise is only sent
  for a window that was floating already: floating a window raises it anyway (0.56
  `floatWindow`; `changeWindowFloatingMode` in older releases).
* **Launch into the pop-out** (Tuner closed: a scheduled switch with Open in: Pop-out starts it
  — `ClosedOutcome::StartApp { popout }` — `tuner --play <ref> --popout`, or `tuner mcp` for an
  agent's play asked for in the pop-out, `--agent-popout`, §30.13): under Hyprland the
  window is created with the app id **`tuner-popout`** (`tuner::window_app_id`, chosen from the
  command line before the window exists; winit can't change it later) at the pop-out size. The
  packaged rule `o.window("^(tuner-popout)$", { float = true, pin = true, no_initial_focus =
  true })` (Hyprland 0.56 Lua rule names; Omarchy's webcam overlay uses the same three) maps it
  floating and pinned on the current workspace without the keyboard. The app pops it out on
  its first frame (also for the reminder launch, so the countdown shows in the pop-out and the
  switch plays in place): size, place, tag — no float, pin or raise. That first pop-out adopts
  the rule's floating + pinned state (`Hypr::with_launch_rule`), so popping in tiles the window
  like any other. The class stays `tuner-popout` for the session; every other `tuner` rule in
  the snippet matches both classes. Without the rule the window maps tiled and focused, then
  pops out as before. Outside Hyprland the app id stays `tuner`.
* **Pop-in** undoes only what pop-out changed: unpin (unless it was pinned), untag (unless it
  was tagged), then back to its floating geometry if it was floating, else tiled — on the
  workspace the user is on now (a pinned window travels). If the user moved or resized the
  pop-out (more than 2 px off its default place), its place and size are remembered
  (`player.popout_last`, relative to the monitor, clamped on reuse); an unmoved pop-out
  clears it, so the next one follows the corner on whichever monitor (resolution) it is.
  Changing size or corner in Settings forgets it too. A refused pop-in keeps the record and
  the window popped out. Toggling while popped out by Super+O (no record) does what Super+O's
  second press does: unpin, untag, tile.
* **State**: the `hypr-events` thread reads `.socket2.sock` and reports our window's
  floating / pinned flags (`changefloatingmode>>ADDR,0|1`, `pin>>ADDR,0|1`; `openwindow`
  re-looks our window up, `closewindow` forgets it). They drive the video-only layout
  (§12.3) and end our pop-out when someone else unpins or tiles it (Super+O pressed again,
  Super+V).
* **Outside Hyprland** (no `HYPRLAND_INSTANCE_SIGNATURE`) the key only toasts that the
  pop-out needs Hyprland. Failures are toasted ("Couldn't pop the player out" / "Couldn't put
  the player back"; the refused dispatcher goes to the log) and leave the layout docked. The
  worker and the event watcher start with the first frame of a real window, so unit tests,
  integration tests and benches never talk to a compositor: `HyprIpc` has a fake that answers
  like Hyprland in either syntax, and the app takes a recording `PopOutBackend`.
* A popped-out window is always visible, so its playback never meets the hidden-workspace
  limits of §11.1.

### 21.7 Startup failures (`platform/startup.rs`)
Tuner starts from the launcher (uwsm / `gtk-launch`): stderr goes nowhere. In 0.2.6 a database
left at schema v14 by a dev build was refused with one stderr line and exit 1 — nothing on
screen, nothing in the log (the error was printed by `main`'s return, never logged), so the
release looked broken. Now `main` runs everything in `run` and hands an error to
`startup::report`, which says it three ways and exits 1 (where it may report is process state
that `run` fills in — `notify_desktop`, `log_to`, `after_update` — so the updater's thread can
report too):
* **stderr** — `Error: …` with the cause chain, as before (redacted).
* **`tuner.log`** — `ERROR tuner: Tuner exits: …` through `logging::record_fatal`: the
  subscriber's writer (flushed) once logging is up, else appended directly. Known as soon as
  the profile's paths are; an error before that (no `$HOME`) has stderr and the desktop only.
* **A desktop notification** (critical, `platform::notify`, redacted): title *Tuner couldn't
  start*, body `startup::user_message` — the reason in plain words and what to do. Known causes
  anywhere in the chain have their own words: a newer schema (below), a damaged, busy,
  read-only database or a full disk (`rusqlite` error codes), a profile folder it can't use
  (`Error::Io`), libmpv failing to initialise (`Error::Mpv`), no window
  (`startup::WindowError`: no display, no GL); anything else (a background refresh holding the
  instance lock, an instance that doesn't answer, a refused handoff) is its own message as a
  sentence. Only for launches that would open a window or show a closed reminder's note — not
  `--refresh` / `--rearm` (systemd's journal has stderr; the next window launch says it) nor
  `tuner mcp` (its client reads stderr; it touches neither the log nor the desktop) — and only
  until eframe's creation callback ran (`startup::window_up`): a winit error at logout is no
  failed start. No notification server: nothing more happens, reporting never panics.

**Newer schema.** The refusal stays (opening it would lose data); it is typed
(`Error::NewerSchema { found, supported, opened_by }`). Every `Store::open` records its build in
`meta.app_version` (`migrations::record_app_version`, written only when it changed; `meta` exists
since v1, so no schema bump) and the refusal reads it back, quoting it only if it looks like a
version. Body: "Your library was last opened by Tuner 0.3.0 (database version 14; this is 13).
Install Tuner 0.3.0 again, or a newer one." — or "…by a newer Tuner…" / "Install that version
again, or a newer one." for databases written before the record existed. Future builds must
keep writing the key.

**Not covered:** a missing `libmpv.so` fails in the dynamic loader before `main` (the package
depends on `mpv`); a panic keeps Rust's report on stderr (a panic hook would also fire for the
panics the store writer and services catch on purpose).

**After an update (§41.6).** The restart re-execs `/usr/bin/tuner` in the same process, so no
old process is left to watch the new build; the new one reports for itself. `run` reads the
restart marker (`update::restart::peek_marker`: its own pid, ≤ 2 min; only read — the update
service still takes it for its "Tuner updated" toast) right after the paths, before anything
else can fail, and `startup::after_update` keeps its `from`/`to` (only if they look like
versions). A failed start then reads "Tuner was updated to 0.2.8 but couldn't start: ‹reason›
To go back to 0.2.7: sudo pacman -U /var/cache/pacman/pkg/omatuner-0.2.7-*.pkg.tar.zst, then
turn off automatic installs in Settings ▸ About & logs ▸ Updates." Tuner never rolls back by
itself: the new build may already have migrated the database, and the older one would refuse
it (naming the newer version, thanks to `meta.app_version`). An `exec` that fails in the old
process — `main` after the window closed, or the updater's thread for a hidden window — is a
`startup::RestartFailed`: reported the same way under *Tuner couldn't restart* ("Tuner was
updated to X but couldn't restart into it (‹why›). Start Tuner again from the launcher."),
although the window was up, and the process exits 1.

---

## 22. Packaging & licence

* **Licence**: GPL-3.0-or-later (Arch's `mpv`/libmpv is built with GPL components, making the
  combined work GPL; MPL-2.0 deps (nucleo, mpris-server) are compatible).
* `packaging/tuner.desktop` (MimeType M3U handlers, actions Guide/Live, `StartupWMClass=tuner`),
  `packaging/icons/tuner.svg` (square-cornered; PKGBUILD installs to hicolor/scalable — PNG
  sizes can be rendered with `rsvg-convert` if a theme needs them), `packaging/hypr/tuner.lua`
  (`-default-opacity` tag + `opacity "1 1"`, **`render_unfocused = true`** so playback keeps
  rendering on other workspaces (§11.1), for classes `tuner` and `tuner-popout`; `float`, `pin`,
  `no_initial_focus` for `tuner-popout` (§21.6); optional idle fallback and Super+Shift+T
  binding),
  `packaging/omarchy/omarchy-menu.jsonc` (TV submenu), `packaging/systemd/tuner-rearm.service`
  (the login re-arm of the reminder timers, §36.2), `packaging/PKGBUILD` (x86_64 + aarch64,
  depends mpv/xz/fontconfig/JetBrains Mono Nerd/wayland/mesa; builds the local checkout into
  `packaging/src/target`, installs the snippets under `/usr/share/tuner/`, the user unit under
  `/usr/lib/systemd/user/` with its `graphical-session.target.wants/` link (enabled for every
  account, owned by the package; pacman's `daemon-reload-user` hook reloads running user
  managers), the docs under `/usr/share/doc/tuner/` and `LICENSE` under
  `/usr/share/licenses/omatuner/`).
  `packaging/omatuner.install` (`install=`, next to the PKGBUILD so makepkg finds it in CI's copy
  of the checkout too) only echoes where to connect an AI agent (§39.8), and, once, on the
  upgrade that brings casting (from before 0.2.6), "Cast to your TV: Settings ▸ Cast (ufw users:
  Tuner offers an Allow on firewall button)"; it runs nothing.
* **Casting packaging** (§40): `ffmpeg` is in `depends` (mpv already pulls it; this declares the
  CLI casting runs), `polkit` is an `optdepends` (the *Allow on firewall* button's `pkexec`).
  The crate's helper binary `tuner-firewall` (`[[bin]]`, std only), installs as
  `/usr/lib/tuner/tuner-firewall` (0755), and `packaging/polkit/io.github.rthoms334.omatuner.
  firewall.policy` as `/usr/share/polkit-1/actions/…` (the action's `exec.path` annotation names
  that exact path). `THIRD_PARTY_NOTICES.md` (pyatv's MIT notice for the ported AirPlay code)
  installs beside `LICENSE` in `/usr/share/licenses/omatuner/`. Nothing else changes for either
  architecture. The release tarball (`omatuner-<pkgver>-<arch>.tar.gz`) does not carry the
  helper: a user without the package gets the generic polkit action (cast design §9.4).
* **Names**: the repository (<https://github.com/rthoms334/omatuner>) and the Arch package
  (`pkgname=omatuner`) are *omatuner*; everything the user sees or configures keeps *tuner*: the
  app name "Tuner", the command `/usr/bin/tuner`, the desktop entry/icon/app id `tuner`, the
  config/data/state/cache folders, the MPRIS name and `/usr/share/{tuner,doc/tuner}`. No
  `provides`/`conflicts`: no package of this app was ever published under another name.
* **Releases** (`.github/workflows/release.yml`): a pushed tag `v<version>` or
  `v<version>-<suffix>` (`<version>` must equal `Cargo.toml`'s; the PKGBUILD's `prepare()` checks
  `pkgver` against it too; the suffix is lowercase letters, digits and dots starting with a letter,
  and a pre-release builds `pkgver=<version><suffix>`, e.g. `0.1.0rc4`, which pacman orders
  before `0.1.0`) builds the PKGBUILD with makepkg as an unprivileged user in
  `archlinux:base-devel` (x86_64, on `ubuntu-24.04`) and `menci/archlinuxarm:base-devel`
  (aarch64, natively on `ubuntu-24.04-arm`; pinned by digest in the release workflow, floating in
  CI), and publishes a GitHub Release (`--generate-notes`; a tag with a suffix is a pre-release;
  re-running a failed publish re-uploads into the existing release) with
  `omatuner-<pkgver>-<rel>-<arch>.pkg.tar.zst`, `omatuner-<pkgver>-<arch>.tar.gz` (binary, desktop entry, icon, snippets, `systemd/tuner-rearm.service`, README, LICENSE, THIRD_PARTY_NOTICES.md) and
  `SHA256SUMS`, and a minisign signature (`<file>.minisig`, pre-hashed) of every package and
  tarball: the publish job signs with the `MINISIGN_SECRET_KEY` / `MINISIGN_PASSWORD` secrets
  (the password on stdin) and fails unless `packaging/update-key.pub` verifies each signature
  and each is pre-hashed (`ED`, the only kind Tuner accepts; §41.3). A manual run with `publish=false`, and every pull request that touches
  `packaging/`, `Cargo.*` or the workflow, is a dry run (workflow artifacts only, unsigned).
* **Updates packaging** (§41): the crate's helper binary `tuner-update` (`[[bin]]`; std,
  `minisign-verify` and `rustix`, sharing `src/update/verify.rs` through `#[path]`, never the
  library), installs as `/usr/lib/tuner/tuner-update` (0755) and its policy as
  `/usr/share/polkit-1/actions/io.github.rthoms334.omatuner.update.policy` (the actions'
  `exec.path` annotation names that exact path; `exec.argv1` picks `install` or
  `passwordless`). `polkit` is an `optdepends` (pkexec). The package writes nothing to `/etc`:
  the passwordless rule exists only once the user turns the switch on. `check()` runs the helper
  once as the build user (it must refuse with exit 2). The tarball doesn't carry the helper: a
  Tuner from it only tells.
* **CI** (`.github/workflows/ci.yml`): pull requests and pushes to `main`, same two containers:
  `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo build
  --all-targets --locked`, `scripts/gen-test-media.sh`, then `dbus-run-session -- cargo test
  --locked` (MPRIS and notification tests need a session bus; mpv runs with `vo=null`/`ao=null`;
  kittest UI tests are headless). The soak test stays `#[ignore]`d.
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
`redact::clear_secrets`. Tests never touch the user's desktop: `platform::off_desktop()` (unit
tests, any process that built a `TestUi` — it calls `platform::keep_off_desktop()` — or
`TUNER_NO_DESKTOP` set) makes `hypr::focus_self` a no-op and lets notifications reach only an
explicit test bus.

### 23.1 Dev notes: README screenshots

`scripts/screenshots.sh [shot…]` regenerates `docs/screenshots/*.png`. The repository is
public, so every image shows made-up content only:

* **Demo profile** — `cargo run --example demo_profile -- <dir> [--media <file>] [--media2 <file>] [--silent]`:
  three fictional playlists (Northwind, Aurora, Harbor Free; distinct colours) as local M3U
  files imported by the real `RefreshSource` job, ~60 channels with quality badges in
  country-tagged categories (`UK | News`, `FR | Cinéma`…), a guide from an XMLTV file
  (`RefreshEpg`) covering yesterday 06:00 to three days ahead (with fixed football fixtures
  tomorrow and the day after), movies and series whose titles repeat across (and inside)
  playlists so "All playlists" stacks them, details written with
  `store::import::store_movie_details`, and favourites, history, progress and reminders (one an
  automatic switch) through `store::user`. For the v0.2 shots: Up next (a film trilogy and an
  episode, `store::queue`), three Following rules (a team, a daily news flash on a bulletin
  that isn't due within the next three hours, a listed show; evaluated by `EvaluateFollows`),
  two channels no guide lists and a "+1" channel (Guide fixes), and a one-connection limit on
  Northwind and Aurora (multi-view takes the second picture's copy from the other playlist). Logos (monograms) and posters/backdrops (gradient landscapes + title)
  are rendered with resvg using egui's bundled fonts and written into the profile's image
  cache under `https://img.tuner-demo.invalid/…` URLs, which are never fetched. Content is
  seeded; only the guide follows today's date. `demo/refs.env` names playlist ids and `--play`
  references for the script. `tests/demo_profile.rs` builds it and checks the library.
* **Capture** (`src/devshot.rs`, environment only, nothing in `--help`):
  `TUNER_SCREENSHOT=<png>` captures the window with `ViewportCommand::Screenshot` and exits;
  `TUNER_SCREENSHOT_SIZE=WxH` first floats, sizes and centres the window over Hyprland IPC
  (`platform::popout::Hypr::float_centred`, found by pid); `TUNER_SCREENSHOT_SCRIPT` runs
  steps (`view:`, `play:`, `key:` — a plain printable key also sends its `Text`, as a
  keyboard does —, `text:`, `move:`, `click:`, `size:`, `categories:`, `wait:`) 0.35 s apart once the first library is in and the window has its size;
  `TUNER_SCREENSHOT_AFTER` (default 2 s) waits after the last step. The PNG is scaled to the
  window's logical size, so the monitor scale doesn't change the result.
* **Fixed theme** — `TUNER_THEME_DIR=<dir>` loads that Omarchy theme directory instead of the
  live one (no `~/.config/omarchy/shell.toml`, rounding 0, no theme watcher). The script uses
  the bundled fixtures `tests/fixtures/theme/tokyo-night` (dark) and `catppuccin-latte`
  (light). Fonts still come from fontconfig (Omarchy: JetBrainsMono Nerd Font).
* **Video** — public-domain NASA footage (NASA Image and Video Library), no people, captions
  or overlays: 8 s of *Earth Views from the International Space Station* (0:49.3–0:57.3,
  clouds over the limb between two cross-fades) on most channels, and 9.5 s of *Artemis I
  Launch (Press Site)* (0:53–1:02.5, SLS clearing the towers) on the channels in the demo
  profile's `SECOND_CLIP` (`--media2`; Lumen Docs, so multi-view's two pictures differ). The
  script downloads both 1280×720 sources into `target/screenshots/media/` (sha256-checked,
  never committed), cuts them with a silent audio track (the Earth Views soundtrack isn't
  used), and mpv loops them (`loop-file=inf`, `ao=null` with `--silent`). Without a download
  that channel's clip is an ffmpeg test pattern.

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
| Hidden-workspace playback (no frame callbacks → no `logic`) | `render_unfocused` window rule shipped; controller video watchdog (`vid=no` after 10 s, resumed on the next render); everything correctness-critical off the UI thread; vsync off + `pre_present_notify` pacing so the UI thread never blocks in a swap and keeps answering pings (no ANR dialog, §11.1). WP-07 + WP-13 measure 60 s on another workspace with and without the rule (audio, `frame-drop-count`, CPU). |
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
  — source badge (§13.0, with several playlists) · year · category (primary group name); no plot block, no "Cast:" line, no
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

## 28. Phase 6: category filter (countries & languages) and bulk hide

Base: branch pointer `p6-base` on `feat/category-filter` (from `v0.1.0-rc3`; this section +
the skeleton of §28.9). Where this section disagrees with an earlier one, this section wins
(§13.2 / §13.5 group panes, §13.9 Settings ▸ Hidden and the playlist editor, §13.10 the
onboarding steps, §15 tables — regenerated).

User problem: Xtream (and many M3U) playlists carry hundreds of categories for dozens of
countries ("AR | News", "FR | Sport", "|DE| Filme"…) and the only tool was Ctrl+H on one group
at a time — which also misses every category the provider adds later.

### 28.1 Decisions
1. **A rule, not hide flags — one rule for every playlist** (field fix, §28.3). A *category
   filter* is a keep-list of country / language / region tags plus "keep untagged". **One rule
   is shared by every playlist** (`meta` row `category-filter`), new playlists included; a
   playlist may have **its own** instead ("Only for <playlist>":
   `SourceSettings::{category_filter, own_category_filter}`). It is applied on every library
   build, so categories a provider adds later for unticked tags stay hidden without the user
   doing anything. No rule (the default) = no filtering. (P6 shipped the rule per playlist; the
   field report below is why it is shared now.)
2. **Same rule for live, movie and series categories** of a source (one keep-list per source).
3. **Defaults** (user decision): the user's country (`presets::detect_country`: timezone, then
   locale — GB here) and its main language, plus every English-language tag (GB, US, CA, AU,
   IE, NZ, `lang:en`), plus untagged. Everything else starts unticked; the user can change it
   before saving.
4. **The user's own per-group choice beats the rule**: Ctrl+H on a group always hides it; showing
   a filtered group from Settings ▸ Hidden exempts that one group (`GroupOverride::shown`).
5. **Filtered = hidden, everywhere**: a filtered group behaves exactly like a group the user hid
   (Live / Guide / Movies / Series / Home / search / zap lists / counts), so every existing
   consumer of `Group::hidden` and `LiveIndex::hidden` follows without change. This includes
   favourites in filtered categories (same as Ctrl+H on their group today); Settings ▸ Hidden
   reverses it.
6. **Bulk hide** in group panes: mark several groups, Ctrl+H hides all marked; while the pane is
   filtered, "Hide these N groups" (Ctrl+Shift+H).

### 28.2 Tag detection (`sources::category_tags::detect_tag`)
Input: the provider's **raw** category name (`grp.raw_name`; smart-name cleanup strips the
prefix from `grp.name`, and `identity::normalize_group` squashes separators out of the key).
A name has at most one tag; the first match wins.

**Canonical tags** (`model::CategoryTag`, its string is also the stored form):
country = ISO 3166-1 alpha-2 upper case (`GB`, never `UK`); language = `lang:` + ISO 639-1
(`lang:en`); region = `region:` + id (`region:latam`). Display: `tag_short` (GB → `UK`,
`lang:en` → `EN`, `region:exyu` → `EX-YU`) and `tag_label` (country names from
`presets::country_name`, built-in names for languages / regions). No flag emoji in the UI:
egui's bundled emoji font has no flag sequences — the short code is shown as a badge.

**Grammar** (applied to the tokens of the trimmed raw name — the shared name tokenizer of
§9.6, so smart names and detection agree on what a separator is; a word with joiners that is
not itself listed is split at them for detection: `UK-DOCS`, `UK.Sports`, `UK/US`; ASCII
case-insensitive unless noted):
1. Leading decoration is skipped: separator runs that open no bracket, superscript decoration
   and stray closing brackets (`★ ✦ ◉ • ▎ ┃ ⓧ ❖ # »`, `ᴴᴰ` …). Trailing decoration likewise
   before the trailing forms (`"Sports | UK ◉"` → GB).
2. *Flag*: a leading pair of regional-indicator symbols → that country, read as an ISO code
   first (`🇬🇧` → GB, `🇦🇷` → Argentina, not Arabic; `🇪🇺` → Europe via the synonyms); the
   England / Scotland / Wales tag sequences (`🏴 + tags`) → GB.
3. *Bracketed token* at the start: `[X]`, `(X)`, `{X}`, `|X|` (also `| X |`), where X is a
   token or a flag (`[🇬🇧] Sports`).
4. *Separated token* at the start: a token followed by optional spaces and a **separator** —
   any punctuation or symbol run of the tokenizer (so `|`, `:`, `-`, `–`, `—`, `/`, `▎`, `✦`,
   `❖`, `★`, `ⓧ`, `•`, `·`, `»`, `>`, `*`, `~`…; not `( & ' +`, which belong to names). `"UK | Sports"`, `"UK: News"`,
   `"UK - Docs"`, `"EN ▎Movies"`, `"UK ✦ Kids"`, `"UK/US Sports"` (first tag wins). A `.` or a
   `-` with no space before it and a letter after it joins words (`"Sci-Fi"`, `"No. 1"`) and
   only separates after a short code accepted by rule 6 (`"UK-DOCS"`, `"UK.Sports"` → GB;
   `"AL-JAZEERA"` stays untagged).
5. *Whole name* is a token: `"ENGLISH"`, `"FRANCE"`, `"EX-YU"`, `"UK"`.
6. *Space-only separator* (`"UK SPORTS"`, `"ARABIC NEWS"`) is accepted only for (a) full names
   (≥ 4 letters: ENGLISH, FRANCE, ARABIC, LATINO, AFRICA…) — in mixed case only country and
   region names, not languages or nationality adjectives (`DEMONYMS`: INDIAN BRITISH IRISH
   AFRICAN ASIAN LATIN CARIBBEAN NORDIC), so `"Latino Peliculas"` is Latin America but
   `"German Bundesliga"`, `"French Open"`, `"Indian Premier League"` stay untagged; (b) codes
   written in upper case in the raw name: two-letter codes only from an allow-list of usual
   IPTV prefixes (`SPACED_CODES`: UK GB US CA AU NZ IE EN AR DE FR ES PT NL TR GR RU … EU AF
   YU), three-letter codes unless they are English words (`AMBIGUOUS`: CAN SPA BRA PER MAR DEN
   FIN RUM…). `"IT Crowd"`, `"Us Weekly"`, `"In Cinemas"`, `"AL JAZEERA"`, `"BT Sport"`,
   `"FA Cup"`, `"CL Matches"`, `"CH 5"` stay untagged; `"IT: Sport"` is Italy.
7. *Non-tags* (and plain numbers) are skipped — bracketed, separated or followed by a space —
   and detection continues with the next token (at most 3 leading tokens): the quality,
   format and service words shared with smart names (`4K HD FHD UHD 1080p… 8K HDR HEVC H265
   50fps VIP SAT RAW PPV 24/7`, also as decoration `ᴴᴰ`) and `SD VOD NEW XXX ADULT 18+ TV FM
   LIVE RADIO MULTI INT INTL INTERNATIONAL WORLD TOP PREMIUM EX`. So `"VIP | UK | Sports"`, `"VIP UK | Sports"` and
   `"[4K] UK: Movies"` → GB; `"4K | Sports"` → untagged; `"NEW ZEALAND"` → NZ (`ZEALAND` is
   listed); `"EX YU"` → Ex-Yugoslavia.
8. *Trailing* forms, only when nothing leads: a bracketed last token (`"Sports (UK)"`,
   `"Movies [EN]"`) or a separated last token (`"NETFLIX - EN"`, `"Sports | UK"`), trailing
   non-tags skipped (`"Sports | UK | HD"`). No space-only form at the end; `PL` and `CL` are
   not read at the end (`"Football | PL"` is the Premier League; `"PL | Polsat"` is Poland).
9. A token is a tag only when it is in the tables below — unknown words never become tags
   (`"CNN: Live"`, `"SKY | Movies"`, `"NBA: Games"` are untagged). Tokens may contain one inner
   `-` or `/` when the whole is listed (`EX-YU`, `24/7`). A `-` between two letters is a
   hyphenated word, not a separator (`"Sci-Fi"`, `"Re-Runs"`, `"Al-Jazeera"` are untagged;
   `"UK - Docs"` is GB).

**Synonyms** (token → tag; full table in code, tested):
| tokens | tag |
|---|---|
| UK, GB, GBR, BRITAIN, BRITISH, ENGLAND, SCOTLAND, WALES, 🇬🇧, 🏴 England/Scotland/Wales | `GB` |
| US, USA, AMERICA | `US` |
| CA, CAN, CANADA · AU, AUS, AUSTRALIA · IE, IRL, IRELAND, IRISH · NZ, NZL, ZEALAND | `CA` · `AU` · `IE` · `NZ` |
| EN, ENG, ENGLISH | `lang:en` (ENG is English, not England: providers use it for the language) |
| AR, ARA, ARAB, ARABIC | `lang:ar` (IPTV convention; Argentina is ARG / ARGENTINA → `AR`) |
| any other ISO 3166-1 alpha-2 code (embedded list), DEU/GER, FRA, ITA, ESP/SPA, POR/PRT, BRA, MEX, TUR, … (the three-letter set of `smart_names::COUNTRY3`), and English country names (FRANCE, GERMANY, DEUTSCHLAND, ITALIA, ESPANA, PORTUGAL, BRASIL, TURKEY/TURKIYE, POLAND/POLSKA, NETHERLANDS/HOLLAND, INDIA, PAKISTAN, …) | that country |
| FRENCH, GERMAN/DEUTSCH, ITALIAN, SPANISH/ESPANOL, PORTUGUESE, TURKISH, RUSSIAN, POLISH, HINDI/HIN, URDU/URD, TAMIL/TAM, TELUGU/TEL, PUNJABI, BANGLA/BENGALI, KURD/KURDISH/KU, PERSIAN/FARSI/PER/FA | `lang:…` |
| LAT, LATIN, LATINO, LATINA, LATAM | `region:latam` |
| EX-YU, EXYU, YU, BALKAN(S) | `region:exyu` |
| AF, AFR, AFRICA, AFRIKA, AFRICAN (`AF| NIGERIA`; Afghanistan is AFG) · ASIA, ASIAN · CAR (separated or bracketed only: `AMBIGUOUS`), CARIB, CARIBBEAN · NORDIC, SCANDINAVIA, SCANDI · EU, EUROPE | `region:africa` · `asia` · `caribbean` · `nordic` · `europe` |
| SW (Sweden on IPTV panels: `SW ★ …`), MAL (Malayalam), TELEGU (sic), ODIA; full names seen as prefixes in real playlists: URUGUAY, DOMINICAN(A), ECUADOR, HONDURAS, GUATEMALA, PANAMA, CUBA, UGANDA, TANZANIA, MALI, GUINEE/GUINEA, BURKINAFASO, TOGO, BENIN, GABON, TCHAD/CHAD, GAMBIA, SIERRALEONE, CAPEVERDE, ANGOLA, MOZAMBIQUE, NAMIBIA, ERITREA, DJIBOUTI, COTEDIVOIRE | `SE` · `lang:ml` · `lang:te` · `lang:or` · those countries |

**Script fallback** (`category_tags::script_tag`, only when the grammar finds no tag): a name
whose letters are mostly (more than half, at least two) in a script one language uses gets
that language — Arabic script → `lang:ar` (Persian, Urdu and Kurdish names count as Arabic),
Greek → `lang:el`, Hebrew → `lang:he`, Thai → `lang:th`, Hangul → `lang:ko`, Kana →
`lang:ja`. Latin, Cyrillic, Han and Devanagari say too little and are never read this way.
Field report: about 500 series and 86 films in untagged Arabic-named categories ("مسلسلات
شاهد الأصلية", "الجزيرة الوثائقية") showed through an English rule with untagged kept;
"VIP ✪ كأس العالم 2026" is `lang:ar` too. Tags are read when the library loads, so this
needs no refresh.

Language names are not merged into countries (`FRENCH` ≠ `FR`): the picker lists both, and
the defaults add the main language of the user's country (`country_language`, a small table:
FR→fr, DE/AT→de, ES/MX/AR…→es, IT→it, PT/BR→pt, NL→nl, TR→tr, PL→pl, RU→ru, IN→hi, PK→ur…).

### 28.3 Rule storage and semantics
* **Field report (v0.1.0):** "When I view Movies I see stacked cards from other languages when
  my settings clearly have show my country (English)." The rule was per playlist and offered
  once, in the add-playlist flow (dismissable with "Show everything"); the user had saved the
  English set on "TV" only, so "TV 2" (no rule) showed every country — about 147 k visible
  non-English copies, 99 % of the foreign stack members and every all-foreign stack of All
  playlists (43 % of All's cards were another language). Tag detection on `✪` names and the
  stacks were right: no hidden copy was in any stack. Hence one shared rule, and §28.12 for the
  foreign titles inside kept / untagged categories (about 27.7 k copies even with TV's rule on
  TV 2). Measured on a copy of that profile after this fix: All has 39.6 k cards, 6 of them
  not English; no all-foreign stack.
* **The shared rule:** `meta` row `category-filter` = JSON `CategoryFilter` (absent = no
  rule); `store::sources::{shared_category_filter, set_shared_category_filter}`.
* **A playlist's own rule:** `SourceSettings::own_category_filter: Option<bool>` +
  `category_filter: Option<CategoryFilter>` (settings JSON). `Some(true)` = `category_filter`
  is its rule (`None` there = show everything of it); `Some(false)` = the shared rule; `None`
  = written before the rule was shared: a stored `category_filter` is its own, none = shared
  (`SourceSettings::{has_own_category_filter, effective_category_filter}`). Written only by
  `store::sources::set_category_rule(id, CategoryRule::{Shared, Own(rule)})`
  (`set_category_filter(id, f)` = `Own(f)`).
* **Adopting old rules** (`sources::adopt_legacy_category_filters`, run by `Store::open` after
  the migrations and by backup restore; no schema migration, so no `user_version` number is
  taken): playlists with a stored `category_filter` and no `own_category_filter`. Without a
  shared rule, the first one (playlist order) becomes it (`known` = what the playlists with the
  same choice knew); each such playlist then follows the shared rule when its choice is the
  same (`CategoryFilter::same_choice`: `keep`, `keep_untagged`, `hide_other_languages`), else
  keeps its rule as its own. So "one playlist has a rule, the others none" (the field report)
  → that rule covers every playlist on the first start; several different rules stay per
  playlist, the first seeding the shared one. When the shared rule is made and some playlist
  had no rule, the tags those playlists have now join its `known` (not badged "new"), and the
  next window toasts once (`meta` `category-filter-adopted-unseen`,
  `sources::take_adopted_category_rule`, taken by `Services::start`): "Your choice of
  countries from TV now applies to all your playlists · titles in other languages hidden ·
  Settings ▸ Hidden items". Their `category_filter` stays stored (ignored
  while they follow the shared rule) so an older build still filters. Idempotent.
* `CategoryFilter { keep: BTreeSet<CategoryTag>, keep_untagged: bool (default true),
  known: BTreeSet<CategoryTag>, hide_other_languages: Option<bool> (§28.12) }`. `known` = the
  tags present (in the playlists the rule covers) when the rule was last saved; the picker
  marks other tags "new" (no toast).
* `CategoryFilter::keeps(tag)`: tagged → `keep` contains it; untagged → `keep_untagged`.
  **Tags that first appear later are hidden** (keep-list semantics) unless they are in `keep`
  — the defaults store the whole English set, so e.g. a later `NZ |` category is shown.
* Written only by `store::sources::set_category_rule` (read-modify-write in the writer).
  `update_source` and `set_source_settings` **preserve** the stored `category_filter` and
  `own_category_filter` (a source editor opened before a picker save must not wipe it — same
  race as §27.3).
* Per-group precedence (`library::apply_category_filter`): the user's hide
  (`GroupOverride::hidden`) > the user's show (`GroupOverride::shown`) > the rule > shown.
  `Group::hidden` is the effective flag; `Group::user_hidden` is the user's own hide (kept
  apart so a group hidden by both still counts as hidden by the user); `Group::filter`
  (`Kept` / `Hidden` / `Exempt`) says what the rule did; `Group::tag` is the detected tag.
* Hiding a group clears `shown`; showing a group sets `shown` only when the rule hides it
  (`store::user::hide_groups`, `show_group(…, exempt)`), so an unhidden group that the rule
  keeps does not become a permanent exception.
* Items: unchanged rules (§5.4) — hidden when hidden themselves or when **all** their groups are
  hidden; a channel in a kept and a filtered group (M3U `;` lists) stays visible.
* Backup: settings JSON, `group_override.shown` and the shared rule
  (`Backup::shared_category_filter`, serde default) round-trip (`GroupOverride` has
  `#[serde(default)]`, older backups restore with `shown = false`). Replace takes the backup's
  shared rule; Merge only when there is none here; then old rules are adopted (above).

### 28.4 Where the rule is applied
* **Import**: `ImportGroup::raw_name` → `grp.raw_name` (migration `0002_category_filter`:
  `grp.raw_name TEXT`, `group_override.shown INTEGER NOT NULL DEFAULT 0`). Groups imported
  before P6 have `raw_name` NULL until the next refresh; detection then falls back to `name`
  (finds tags only when smart names are off) and the picker shows "Refresh to find all
  countries" (`CategorySummary::needs_refresh`). A refresh that reaches such a group only
  through an item (the provider no longer lists the category) stores `name` as its raw name,
  since the real one cannot be learned; groups of a kind the refresh did not import stay NULL.
* **Library build** (`store::library::load`, off the UI thread): `load_sources` reads the
  shared rule and parses `settings` → `SourceMeta::category_filter` (the rule that applies:
  own, else shared) and `own_category_filter`; `GROUP_COLS` gains `g.raw_name` and `o.shown`;
  `group_from_row` sets `tag = detect_tag(raw_name ?? name)` and `filter = Exempt` when
  `shown`, else `Kept`; `load_groups` calls `apply_category_filter` with the group's source
  rule **before** `group_flags`, so `members`, `collection`, `visible_groups`, `by_number`, the
  VOD vectors and zap lists all exclude filtered groups' items. Cost: one detection per group
  per build (a few thousand short strings, < 5 ms).
* **Search** (`search.rs`): reads `Group::hidden` and `LiveIndex::hidden` — no change; a test
  pins that filtered items are not indexed.
* **Counts**: group panes count visible members (unchanged code). Settings ▸ Playlists keeps
  the imported totals and adds "Showing 4 of 41 countries" for a filtered source.
* **Settings ▸ Hidden** (§28.5).
* `store::live::groups` / `group` (point lookups) return the user's override only (no rule):
  they are used by tests and point reads, never for lists.
* Writes: picker save → `set_category_filter` + `DataScope::Sources` (rebuilds the library and
  reloads the source editor); bulk hide / show → `DataScope::Overrides`.

### 28.5 Settings ▸ Hidden
* **Hidden by the category filter** — one section for the playlists sharing the rule ("Your
  playlists", keyed by the first of them) and one per playlist with its own rule; Enter on a
  tag adds it to the rule that hid it (`sources::keep_tag`: own, else shared — toast "Arabic
  shown in every playlist"); an "N titles in other languages" row (§28.12,
  `SourceMeta::language_hidden`) opens the picker. Per section: a row per hidden tag
  ("Arabic · 112 categories · 4 380 channels"), Enter = **show this country** (adds the tag
  to `keep`; toast "Arabic shown"), → expands the tag's categories in place (Shift+← collapses;
  ← is the page list);
  Enter on a category row shows just that one (`show_group(…, exempt: true)`). Expanded tags
  show a down chevron and their category rows' detail starts with the tag ("Arabic · Live TV ·
  Enter shows just this one"). A last row "Choose countries…" opens the picker. A playlist
  with ≥ 2 tags and nothing filtered gets a section with just "Choose countries…".
* **Hidden groups**: only groups the user hid (`user_hidden`), Enter unhides
  (`show_group(…, exempt: filter != Kept)`).
* **Hidden channels**: only channels hidden by their own override, i.e. channels of
  `LiveIndex::hidden` that have at least one visible group or no group (a filtered source would
  otherwise list thousands).

### 28.6 "Choose what to show" (the picker)
* **Edits the shared rule**, opened from one playlist. Its first rows (listed while the filter
  is empty; Space ticks them like any row; Ctrl+A ignores them): **Only for <playlist>** (two
  or more playlists, or the playlist already has its own rule) — ticked, the rows become that
  playlist's tags and Enter stores its own rule (`CategoryRule::Own`); unticked, Enter stores
  the shared rule and the playlist follows it (`CategoryRule::Shared` when it had its own) —
  and **Also hide titles in other languages** (§28.12; when any playlist has movies or
  series). The shared rule's ticks and the playlist's own are two sets (`Ticks`: keep,
  untagged, title option): "Only for" first ticked starts the own ticks from the shared ones; unticked, the shared ticks are back as they were. A playlist that
  had its own rule and only has "Only for" unticked **follows** the shared rule on Enter, which
  is not rewritten (`Save::Follow`); the shared rule is written only when its ticks changed.
  Without a shared rule such a playlist sees "everything" ticked as the shared choice (what
  following shows). "Only for" is offered with two or more **enabled** playlists.
  A line under the list says who it applies to ("Applies to every playlist, also ones you add
  later…" / "Only TV 2 — …"); the save toast ends "in every playlist" / "in TV 2". No keymap
  change (the options are rows).
* Data (`overlays::categories::load`): the shared rule's summary =
  `library::category_summary_of(conn, [playlists following it + this one])` (counts and
  `known` aggregate across playlists), this playlist's own `category_summary(conn, source)`,
  the shared rule and its own rule. (loader, off the UI thread; reloaded on
  `DataScope::Live | Vod | Sources` while open, so Xtream VOD categories that import after the
  channels join the list) → `CategorySummary { tags: Vec<TagSummary>, needs_refresh }`; tagged
  entries by item count descending, then "No country tag". Each row: short-code badge, label,
  counts ("38 live groups · 2 140 channels · 12 movie / 9 series categories"), a "new" badge for
  tags not in `known`, a tick box.
* Initial ticks: the rule edited (its own when it has one, else the shared one); a playlist's
  own "show everything" ticks every row; with no rule, `default_keep(detect_country())` (only the
  present tags are displayed; the stored `keep` is the full default set, see §28.3) and
  untagged ticked.
* Keys (context stack `[Checklist, Modal]`, see the keymap note in §28.7): Space / Insert tick, Ctrl+A
  all / none (of the rows shown by the filter), Ctrl+E "just my country + English" (the
  defaults), type to filter (label + short code; Space is not typable here — tag names are one
  word), Enter **save** (`keep` = ticked tags ∪ (stored `keep` − present tags), so ticks of
  absent tags such as the English defaults survive; `known` = present tags), Esc clears the
  filter, then cancels. Buttons under the list mirror the keys (Select all · None · My country +
  English · Save · Cancel). Saving with every tag and untagged ticked stores `None` (no rule).
  Live: the library rebuild after save applies it. A line above the buttons shows the effect of
  the ticks ("6 of 42 ticked · 3 210 of 48 900 channels"); ticks that keep nothing of the
  playlist add a `warning` line "Nothing from this playlist will show." and the first Enter on
  them only arms it ("— Enter again to save anyway"). Copy says "countries and languages"
  (tags are also languages and regions); the save toast: "Showing 6 of 41 countries and
  languages · Settings ▸ Hidden items shows the rest". The key line under the buttons is
  `keymap::hint_line([Checklist, Modal])` + "Esc cancel" / "Esc show everything".
* **Onboarding** (§13.10): after the Import step's channels phase, the wizard loads the summary
  of the new source; with `tag_count() ≥ 2` it shows the **Choose what to show** step (the
  import continues; its progress line stays visible under the list). Enter saves and returns to
  the import / Done step; Esc ("Show everything") saves nothing. Card copy: "This playlist has
  categories in 41 countries and languages. Pick the ones you want — you can change this later
  in Settings ▸ Playlists." Same for Settings ▸ Add (same card). The step appears on its own
  just as "Open Live TV" turns on, so Enter is ignored for its first 400 ms (an Enter meant for
  that button must not save the defaults unseen); leaving and re-entering the card keeps the
  step. Not offered when a rule already applies to the playlist: with a shared rule the new
  playlist follows it and a toast says so ("TV 2 shows the countries and languages you chose
  for your playlists · Settings ▸ Playlists changes it"); a restored playlist may have its own.
  The step stores the shared rule (one playlist: it is that playlist's choice anyway).
* **Settings ▸ Playlists ▸ <playlist>**: a "Categories…" row (detail: "All countries and
  languages" / "4 of 41 countries and languages") opens the same picker as an overlay
  (`OverlayRequest::Categories(SourceId)`); with `needs_refresh` the picker shows a note ("…
  refresh to find them all (Ctrl+R)") and a "Refresh now" button, and Ctrl+R / F5 in the picker
  runs a **forced** refresh of that playlist (an unforced one may get "304 Not Modified" and
  never write the raw names).
* **Ways in for playlists added before this phase** (the user's case): "Countries &
  languages…" in the playlist row's menu (Settings ▸ Playlists), in the Live group menu and the
  Movies / Series category menu (groups of a playlist with ≥ 2 tags), and Settings ▸ Hidden
  lists "Choose countries…" for every playlist with ≥ 2 tags, filtered or not.

### 28.7 Bulk hide in group panes (Live groups, Movies / Series categories)
* **Marks**: Space (List context, empty filter — a leading space is ignored by the filter
  anyway) or Insert (List and ListFiltering) toggles the mark of the focused provider group and
  moves down; marked rows show a check glyph (`glyphs::CHECK`) and the accent tint. Virtual rows
  (favourites lists, History, All channels…) cannot be marked. Marks are per pane, cleared on
  hide and on Esc (first rung of the Esc ladder); a library rebuild keeps the marks of groups the
  pane still lists (group ids are stable) and drops the others. The tag code also filters
  ("uk" finds the UK groups smart names renamed).
* **Ctrl+H** with marks hides all marked groups in one write (`hide_groups`), toast "Hid 5
  groups · Settings ▸ Hidden items shows them again"; without marks it is unchanged (the focused
  group).
* **Ctrl+Shift+H** (ListFiltering only) "Hide these N groups": the provider groups the filter
  shows **whose names (with the tag code) have a word starting with each word typed** — the
  fuzzy filter also shows "Cartoons", "Documentary" for "ar", which a bulk hide must not take —
  after a Confirm ("Hide 37 groups?", not destructive; the body names the first 5 "… and 32
  more") when N > 1. The pane footer shows "37 groups with “ar” · Ctrl+Shift+H hides them" while
  filtering and "5 marked · Ctrl+H hides · Esc clears" while marks exist (keys from the keymap);
  the context menu (Ctrl+Enter) lists both actions. Movies / Series say "categories".
* A resolved plain `Key::Space` drops its `Text(" ")` twin in `raw_input_hook` (like Alt
  chords): Space choosing a menu item closes the menu in that frame, and the twin would
  otherwise reach the pane below as ToggleMark.
* Ctrl+Space is not used (fcitx5's input-method toggle on Omarchy).
* Group rows show the tag's short code as a muted suffix badge when their source has ≥ 2 tags
  (smart names drop "UK |", so "UK | Sports" and "US | Sports" otherwise both read "Sports"),
  unless the name already shows the code as a word ("|UK| Sports" with smart names off).

**Keymap changes** (`src/keymap.rs`; §15.2 regenerated with `cargo run --example keymap_md`):
new actions `ToggleMark`, `MarkAll`, `MarkDefaults`, `HideShown`; new context
`KeyContext::Checklist` (title "Tick lists"), stacked `[Checklist, Modal]` and filterable
(letters type). It binds its own navigation and filter keys (↑/↓/PgUp/PgDn/Home/End,
Backspace, Ctrl+Backspace, Ctrl+U, Esc → Back "Clear the filter, then cancel") instead of
stacking on `List`, where → would be Activate (save) and Backspace Parent. `Key::Space` / Insert →
ToggleMark "Tick / untick" (the KEY, because `Modal` below binds `Key::Space` to Confirm; the
`Text(" ")` twin is dropped by `raw_input_hook`), Ctrl+A → MarkAll
"Tick all / none", Ctrl+E → MarkDefaults "Just my country + English", Enter → Activate "Save";
hints `tick · all/none · my country · save`.
LIST: `Char(' ')` and Insert → ToggleMark "Mark group (Ctrl+H then hides all marked)";
LIST_FILTERING: Insert → ToggleMark, Ctrl+Shift+H → HideShown "Hide the groups with the words
typed". Space is
bound as `Char(' ')` (the Text event), never `Key::Space`, so it never also types into the
filter. Screens without marks ignore ToggleMark; the jobs overlay (`[List, Modal]`) too.

### 28.8 Not in scope
Per-kind rules, auto-detected language of channel names (no tag in the category), the M3U
`tvg-country` attribute, toasts about new tags after a refresh, flag emoji.

### 28.9 Skeleton API (branch `p6-base`)
Compiled from the start; `todo!("P6-core")` only where nothing calls it yet:
* `model::category::{CategoryTag (country/language/region/kind/code), TagKind, CategoryFilter
  (Default: keep_untagged = true; keeps), GroupFilter {Kept, Hidden, Exempt}}` — implemented.
* `model::SourceSettings::category_filter: Option<CategoryFilter>`; `model::Group::{tag,
  filter}`; `model::GroupOverride::shown`; `model::GroupRef {source, kind, key}` +
  `From<&Group>`.
* `sources::category_tags::{ENGLISH_COUNTRIES, is_english, default_keep, tag_short, TagSummary
  (items, groups), CategorySummary (tag_count, present)}` — implemented; `tag_label`
  (countries only; languages/regions fall back to the short code) and `country_language`
  (returns `None`) — placeholders for core; `detect_tag`, `summarize` — todo.
* Migration `0002_category_filter.sql`; `store::import::ImportGroup::raw_name` (set by the M3U
  and Xtream converters; not yet written by `upsert_group`).
* `store::user::{hide_groups, show_group}` and `set_group_override` / `group_overrides` with
  `shown` — implemented. `store::sources::set_category_filter` — implemented.
* `store::library::{SourceMeta::category_filter (None until core parses it),
  apply_category_filter}` — implemented; `category_summary` — todo; `group_from_row` sets
  `tag: None`, `filter: Kept` until core.

### 28.10 Work split (two parallel tracks)
File ownership is disjoint; a cross-track edit needs the owner's agreement and is listed in the
merge note. Both tracks branch from `p6-base`. Lean: clippy + the tests you touched; the
integrator runs the one full gate.

**core track** — owns `src/sources/**`, `src/store/**` (incl. `backup.rs`, migrations),
`src/model/**`, `src/search.rs`, `src/jobs/**`, `benches/**` (compile fixes only),
`tests/{store_catalogue,store_backup,import_e2e,search,m3u,xtream,resolve,store_epg}.rs`.
1. `category_tags`: `detect_tag` per §28.2 (embedded ISO alpha-2 list, synonym / non-tag /
   ambiguous tables), `tag_label` names for languages & regions, `country_language`,
   `summarize`. Tests (one table-driven unit test, extend it rather than adding many): every
   pattern of §28.2 incl. flags, `|UK|`, `[UK]`, `UK ✦`, `EN ▎`, trailing forms, `VIP | UK |`,
   negatives (`4K | Sports`, `24/7 Movies`, `CNN: Live`, `IT Crowd`, `Us Weekly`, `HD`,
   `XXX`, `ADULT`), synonyms (UK/GB/GBR → GB; EN/ENG/ENGLISH → lang:en; AR → lang:ar; ARG →
   AR; EX-YU/YU → region:exyu); `summarize` counts per kind and order.
2. Import writes `grp.raw_name` = `raw_name ?? name` (`upsert_group` insert + `DO UPDATE`), so
   NULL means "imported before P6" and nothing else.
3. Library: `load_sources` parses `settings` → `category_filter`; `GROUP_COLS` + `group_from_row`
   (`raw_name`, `shown`, tag); `load_groups` applies the rule before `group_flags`;
   `category_summary` (all kinds, enabled or not; `needs_refresh` = any group of the source
   with NULL `raw_name`).
4. `update_source` / `set_source_settings` preserve the stored `category_filter`.
5. Backup: `GroupOverride::shown` exported / restored (Replace and Merge); settings JSON already
   round-trips — extend the existing backup test literal.
6. Tests (extend `tests/store_catalogue.rs`): a source with groups `UK | News`, `AR | News`,
   `Sports` (untagged) and a filter keeping GB → AR group hidden with `filter == Hidden`, its
   channels absent from `collection(AllChannels)` and from a `SearchIndex` built on the
   loaded library, untagged kept; a refresh that adds `|FR| Films` → hidden without touching
   the rule; `show_group(exempt)` on the AR group → visible (`Exempt`); `hide_groups` on a kept
   group → hidden and `shown` cleared; a channel in a kept and a filtered group stays visible;
   `update_source` with a stale copy keeps the rule.

**ui track** — owns `src/ui/**`, `src/keymap.rs`, `src/app.rs`, `examples/keymap_md.rs`,
`tests/{onboarding,guide,widgets,keymap}.rs`, the regenerated §15.2 tables.
1. Keymap (§28.7): actions, `Checklist` context (+ `ALL`, title, `tables`, hints), LIST /
   LIST_FILTERING bindings, Ctrl+H help text. Tests (extend `tests/keymap.rs`): Space in `List`
   → ToggleMark but in `ListFiltering` → FilterInput; `[Checklist, ListFiltering, Modal]` Space →
   ToggleMark, letters → FilterInput, Enter → Activate; Ctrl+Shift+H only while filtering;
   existing "no shadowing" / "help complete" tests cover the rest.
2. Picker: one implementation (`ui/overlays/categories.rs` overlay + a reusable state/view used
   by the onboarding step), `OverlayRequest::Categories(SourceId)` wired in `overlays::build`.
   Tests: defaults pre-tick GB + English + untagged and nothing else; Space / Ctrl+A / Ctrl+E;
   filter then Ctrl+A ticks only the shown rows; Enter saves the expected `CategoryFilter`
   (`keep` keeps absent default tags, `known` = present) via `set_category_filter`; all ticked
   → `None`. Build summaries directly (`CategorySummary`) — `category_summary` is todo until core
   lands.
3. Onboarding step (§28.6): trigger after the channels phase when `tag_count() ≥ 2`; Esc = show
   everything. Test (extend `tests/onboarding.rs`): ≥ 2 tags → step shown, Enter saves; 1 tag →
   no step.
4. Settings: playlist "Categories…" row + status detail; Hidden page sections (§28.5). Test
   (extend the existing Hidden-page test in `settings.rs`): a library with a filtered group shows
   the tag row, Enter shows the tag (writes the rule); user-hidden list excludes filtered groups.
5. Group panes (Live, Movies / Series categories): marks, Ctrl+H on marks, Ctrl+Shift+H with
   Confirm, footer lines, Esc clears marks, tag suffix badge. Tests (extend the existing
   `live.rs` / `movies.rs` screen tests): mark two groups + Ctrl+H → one write hiding both;
   filter + Ctrl+Shift+H + confirm → exactly the filtered groups; Esc clears marks before
   leaving the pane.
6. Help rows / hint lines: generated from the tables; check the status-bar hint for
   `Checklist`.

### 28.11 Risks
* **False positives** hide content the user wants: detection only accepts listed tokens, with
  separators for short codes and the AMBIGUOUS list; untagged is kept by default; every hidden
  tag is one Enter away in Settings ▸ Hidden. The table-driven test is the guard — add every
  reported mis-detection to it.
* **Stale raw names**: groups imported before P6 have no raw name until the next refresh (the
  picker says so); the rule then applies to more groups after that refresh.
* **New-tag surprise**: a provider renaming "UK |" to "GB:" keeps working (synonyms); a provider
  switching to a format we don't parse makes categories untagged → shown (safe side).
* **Editor race** wiping the rule: `update_source` / `set_source_settings` preserve it (§28.3).
* **Favourites in filtered categories disappear** from favourites lists (decision 5) — same
  as hiding their group today; documented in the picker's footnote ("Favourites in hidden
  countries are hidden too").
* **Space binding in `List`**: every list now maps Space to ToggleMark; screens that don't mark
  must ignore it (they do: unhandled actions are no-ops) — keymap test pins it.

### 28.12 Titles in other languages (title-level, field fix)
The category filter works on categories; providers also put foreign titles in categories the
rule keeps or that have no tag (field report: Arabic-script, "SVENSKA", "DANSKE",
"VIAPLAY ÍSLANDS", "EX - MOVIES", "QC - FILMS" categories, "CA ✪ QUEBEC …" under Canada,
"VIP ✪ …" Arabic matches — about 27.7 k copies once TV 2 had TV's rule). Each such title's own
name carries a market prefix (`vod_match`, §29.10), so:
* **Option** `CategoryFilter::hide_other_languages: Option<bool>`: `None` = automatic — on
  when the rule keeps only English-language tags (`category_tags::{hides_other_languages,
  keeps_only_english}`: GB US CA AU IE NZ `lang:en`); the picker's row stores the user's
  choice. A non-English rule is off by default: a country tag does not say one language (BE,
  CH, CA/Québec), so we don't guess for those users.
* **Kept languages** (`category_tags::rule_languages`): the language tags kept, each kept
  country's main language (`country_language`, `IS` → `is` added), Ex-Yugoslav (`sh`) for
  `region:exyu` / RS HR BA ME; plus the user's audio languages (`player.audio_langs`,
  `LoadOptions::lang_prefs`; sr / hr / bs add `sh`).
* **A title's language** (`vod_match::market_lang`): the market its name marks — its audio
  language, or for a subtitled market (`AR-SUBS`, `KU-S`, `NL [SUB]`) the market's language
  (its subtitles and its shelf; such copies are hidden too); `EX` → `sh`; multi-audio and
  regions without one language (`SC`) → none. An **untagged** title in a category whose tagged
  titles are ≥ 80 % one market (≥ 10; `stacks::dominant_market`, same rule as the stacks'
  inference) takes that market ("Casablanca" in an Arabic-subtitled shelf); so does a bare
  subtitle mark with no market (`SUBS - Four Brothers`, `+SUB`: `vod_match::is_bare_subtitles`),
  which also doesn't count as tagged for the category's share.
* **Hidden:** a movie / series of a playlist whose rule hides other languages, with a
  language not kept. **Never hidden:** untagged and multi-audio titles, favourites, and titles
  with watch progress (a series with an episode's) — `library::LanguageRule::protected`.
* **Where:** `library::load_with`, right after `load_vod` (before the VOD index, the explicit
  filter and the stacks): the titles are absent from the snapshot like hidden ones — lists,
  counts (`Group::item_count` lowered; an emptied untagged category stays, empty), search,
  Home, zap lists and **stacks** (hidden copies never stack). `SourceMeta::language_hidden`
  counts them for Settings ▸ Hidden. Live channels are not affected (no per-channel market).
  Cost: one pass over the movies / series (a hash lookup per distinct `match_lang`).
* Tag detection gained the playlists' own-language names seen in the report (`SVENSKA`,
  `DANSKE`, `NORSKE`, `ÍSLANDS` → SE / DK / NO / IS) and the script fallback (§28.2):
  Arabic-named categories are `lang:ar` and hide as categories.
* Settings ▸ Hidden shows only the count ("N titles in other languages"); single titles can't
  be brought back one by one — the row says to untick the option (favourites and started
  titles always show).

## 29. Several playlists (P1: what am I browsing; P2: stacks in All; P3: copies everywhere)

With two providers ("TV" and "TV 2", often both Xtream) nothing said which playlist a list,
card or page came from, two "Action" categories looked alike, and "is there more than one
playlist?" was `sources.len() > 1` — counting disabled playlists and free-channel presets. P1
answers *what am I browsing*; combining duplicate titles in "All" (P2) and the other screens
(P3) come later. In P1 "All playlists" is exactly the old list, plus playlist marks.

### 29.1 Decisions (user, authoritative)
1. Each screen remembers its own playlist: Movies, Series and Live TV; the guide follows Live TV.
2. Free presets stay in Live TV's All, with a per-playlist **"Show in All playlists"** switch.
3. (P2) duplicates combine in All, the copy that plays follows the playlist order — not in P1.
4. (P2, 2026-10-11) Combined titles are **stacks**: a stack card (offset edges, copy count) whose
   detail page lists the copies as mini-cards; Space (or the count badge) fans the stack out
   in the grid; Shift+Enter plays the preferred copy. Same-playlist duplicates stack too.
5. (P2) **One stack across languages and markets**: same title + year; TMDB ids decide when
   known; different ids never stack. Language and dub rules order and label the copies inside.
6. (P2) The **top card** is the copy in the user's audio language (`en` by default), unlocked
   first; with none, the playlist order decides.
7. (P2) **Hidden copies never stack** (category filter, titles in other languages §28.12,
   hidden categories or items,
   playlists out of All, and explicit content behind parental control); **locked copies do**,
   drawn disabled with the lock (PIN on pick); an all-locked stack shows locked; the copy that
   plays skips locked copies unless the session is unlocked.

### 29.2 Eligibility (`Library::eligible(kind)`)
A playlist is *eligible* for a screen when it is **enabled and has that kind of item**
(`SourceMeta::has`: `has_live` / `has_vod` / `has_series`, read with the snapshot's sources).
Every "several playlists?" question uses `eligible(kind).len() > 1`. Per-source visible counts
come from the snapshot (`LiveIndex` / `VodIndex` build them; `Library::count`).

### 29.3 The scope (`ui::scope`)
* `ScopeKind::{Movies, Series, Live}` (`of_screen`: the guide maps to Live). The choice is
  stored in `interface.playlist_scope.{movies,series,live}` (`None` = All) and changed through
  `ctx.edit_config` (`scope::choose`), so it lands at the end of the frame and is saved.
* `Scope::of(lib, scopes, kind)`: the eligible playlists and the one picked — a stored id that
  isn't eligible, or fewer than two eligible playlists, reads as All (the stored id is kept
  while the playlist stays eligible).
* `ScopeFilter`: `Only(id)`, or for All `Except(the playlists whose "Show in All playlists" is
  off)` — `Everything` when that would leave nothing. Screens key their rebuilds on it.
  `Except` narrows only the provider lists (All, categories / groups): the user's own lists
  (favourites, History, Reminders, Continue watching) show every playlist in All
  (`ScopeFilter::for_user_list`) — switching the free presets out of All never hides a
  favourite.
* **Changing the scope** clears the category / group and list filters and starts at the top,
  so a category of the old playlist never stays open under the new name.
* **Disappearing playlist**: on each library swap the app checks the three stored scopes; one
  that is no longer eligible is reset to All with a toast — "TV 2 is disabled — showing All
  playlists", "… was removed …", "… has no movies now …".

### 29.4 What the scope changes
| Screen | One playlist picked | All playlists |
|---|---|---|
| Movies / Series | its categories (custom order, rename, Alt+↑/↓ as before); All, Continue watching and Favourites narrowed to its titles; breadcrumb `TV 2 › Action`; no card marks | every eligible playlist in `in_all`; category rows and cards carry the playlist (§13.0); typing a playlist's name in the category filter finds its categories; breadcrumb `All playlists › Action · TV 2` |
| Live TV | its groups; All channels, favourites lists, History, Reminders and the **zap lists** narrowed to its channels (`live::zap_list_in`); header `TV 2 › UK News`; another playlist's channel (from Search, or what plays) never opens a group the scope doesn't list | groups and channel rows name their playlist in colour; header `All playlists › UK News · TV 2` |
| Guide | follows Live TV: rows, collection picker, zap lists; a group of another playlist falls back; "Open in guide" on another playlist's channel switches Live TV to All (or to that playlist when it is out of All), with a toast, and focuses it | picker groups the groups under each playlist's name |

Search, Home, popovers and the player's "primary group" zaps are not scoped (P1). The player's
channel panel shows playlist names as Live TV does (none while Live TV shows one playlist).

### 29.5 Showing it
* **Scope bar** (`widgets::scope_bar`) on Movies, Series and Live TV when two or more playlists
  are eligible: `All playlists 214 380 · ● TV 151 022 · ● TV 2 98 117` at Wide; the single
  `● TV 2 ▾` button (opens the picker) at Medium / Compact, or when the tabs don't fit. The
  guide has the button before its collection button. Right side at Wide: "Alt+P playlist".
* **Stripe**: 2 px in the picked playlist's colour under the bar (the guide: under its
  header); none for All. The selected tab is underlined only for All (the stripe marks a
  picked playlist).
* **Window title**: `Tuner — Movies · TV 2` while a scoped screen shows one playlist (the
  Omarchy bar shows it), else `Tuner` (`TunerApp::title_now`, sent only on change).
* **Marks** (§13.0 "Playlist on cards"): Auto = two or more eligible playlists for the item's
  kind and a view of All; detail pages (Xtream too) and the player show the playlist whenever
  two or more are eligible.

### 29.6 Choosing
| Input | Effect |
|---|---|
| `Alt+P` | the picker (`overlays::playlists`): All + each eligible playlist with dot and count; ↑/↓ Enter, click, **digits jump** (0 = All, 1–9 in order) while nothing is typed, typing filters; it saves the choice itself |
| `Ctrl+PgDn` / `Ctrl+PgUp` | next / previous (All → TV → TV 2 → All), toast "Showing TV 2 only" / "Showing all playlists" |
| click a tab | that playlist |
| right-click a tab | menu: Edit playlist… (`Route::EditPlaylist` → Settings ▸ Playlists with its editor open), Move up / Move down (the playlist order, also the tab order: the playlist moves past the neighbouring *tab*, skipping playlists without one on that screen) |

Keys are in `GLOBAL` (Ctrl/Alt chords only) and act only on Movies, Series, Live TV and the
guide; with one eligible playlist they say "Only one playlist has movies". Rejected: Ctrl+0
(egui's zoom reset), Alt+[ / Alt+] (AltGr chords on German / French keyboards). The status bar
adds "Alt+P playlist" (before "help") only on those screens with two or more eligible playlists
(`keymap::hint_line_with`); help rows and §15.2 come from the table.

### 29.7 Telling playlists apart
* A new playlist (onboarding, free presets) starts in the **first colour no playlist uses**,
  compared as the theme draws them — Accent is often the theme's blue
  (`scope::first_free_color`).
* Settings ▸ Playlists lists playlists that **look the same** (same glyph and same drawn
  colour, `scope::look_alikes`): "TV and TV 2 share a colour" (their codes always differ), with a
  **Give each its own colour** action that keeps the first of each group and gives the others
  the first free colours (`scope::distinct_colors`; toast). Nothing changes until it is
  pressed.
* Marks are code chips, unique per playlist (§13.0 "Playlist codes"); the name is always in the
  accessible label, the tooltip and the status bar line of the focused item.

### 29.8 Changes to the "playlist on cards" work it folds in
Kept: `PlaylistOnCards`, `SourceMarks` (now `for_view(mode, sources, kind, all)`),
`TileDecor.source`, the detail badge (now on
Xtream pages too). The coloured name on the meta line and the glyph corner chip became code
chips (§13.0 "Playlist codes"). Done in P2 (§29.14), differently: the stack card chips its
playlists (`TileDecor.stack`, no `SourceSet`), the stack edges and copy count, `+1` inside one
playlist, and the detail pages' Copies section instead of `DetailHeader` copies.

### 29.9 Tests
`ui::scope` (eligibility, stored choice, `in_all` incl. the user's own lists, cycling,
look-alikes, free colours), `ui::data` (the Auto rule per kind and scope), Movies (bar,
Ctrl+PgDn + toast, per-screen persistence, filters cleared on a scope change, narrowed
categories / Favourites, breadcrumb in All and in one playlist, finding categories by playlist
name, marks, tab click / right-click, Alt+P), the picker (digits, filter), tab moves past the
neighbouring tab (`overlays::playlists`), Live (groups, All channels and zap lists per scope,
`in_all` vs favourites, painted channel-row names, breadcrumb, another playlist's channel),
the guide (follows Live, grouped picker, "Open in guide" across playlists), Settings
(look-alike fix, edit route, `in_all` saved without a refresh), the app (disabled / removed
fallback toasts, window title, hint), keymap
(chords, hint placement), and the 790 / 1600 pt × text 12 / 16 layout test with Live and the
guide scoped.

### 29.10 P2: matching copies (`sources::vod_match`)

The dedup report on the real catalogue (302k movies, 78k series, two Xtream playlists) and its
skeptical review set these rules; `examples/dedup_report.rs` re-runs the numbers on any
database (read-only: names, years, TMDB ids, category names — never logins, URLs or streams).

**Match key** of a copy, from the provider's RAW name (`movie.name` / `series.name`; smart
names and renames never change it), by `vod_match::parse(name, year, &PrefixCodes)`:

| Step | Rule | Example |
|---|---|---|
| prefixes | up to three leading codes `CODE ★ ` / `CODE - ` / `CODE: ` (`[A-Z0-9+]`, parts joined by `-` or `/`); taken when every part is a known quality / brand / label / marker word, a market code (not with `:`), or (code, separator) opens ≥ 20 names of the playlist (`PrefixCodes`, data-driven like category tags); a chained prefix uses the first one's separator | `EN ★ `, `4K-NF - `, `NF: `, `UK/US - `; `EN - IT: Welcome to Derry` and `IN - KD – The Devil` keep `IT` / `KD`; `IT: Welcome to Derry` alone is a title |
| language / market | the code minus quality, brand and label parts, spelled one way (`MARKET_ALIASES`, data: `ALB`=`AL`, `EAR`=`AR-SUBS`, `TAM`=`IN-TL`, `EG`=`AR-EG`, `SW`=`SE`, `UK`=`EN`…); dub / sub markers join it as `Dub` (`[مدبلج]`, `Dubbed`, `VF` → dub; `VOSTFR`, `[Multi-Sub]`, `VO` → sub; `[Multi-Audio]` → multi) | `FR ★ A Working Man - 2025 [VOSTFR]` → `FR+SUB` |
| rank numbers | `NN.` after a `TOP` label goes | `DE-TOP - 38. Prinzessin Mononoke` |
| brackets | a year is the name's year (`( 2024 )` too); quality / format-only groups and tags go (recorded as `VersionTags`: quality word, HDR, DV, CAM, 3D); a round `(XX)` ISO country is the **origin**, out of the key; any other text stays | `Lewis (2007) (GB)` → key `lewis`, origin GB; `[Extended]`, `(and Everybody Else)` stay |
| year | the `year` column, else the name's (`(2021)`, `- 2021`, a broken `[4k[` end) | |
| title end | quality words come off only at the end (`Sin City HD`), or alone in brackets; `The Vision` keeps its word; `Dolby Vision` at the end goes | |
| markers | a dotted acronym (`S.W.A.T.`) marks the key (never meets `Swat`); a `3D` prefix part too | `swat|acr`, `avatar|3d` |
| fold | lower-case letters and digits; Latin diacritics folded (`Opération` = `Operation`), other scripts kept | |

Audio language of a tag (`vod_match::audio_lang`): the market's language; `IN-xx` are Indian
languages (`IN-TL` Tamil…), and `IN-EN` is the **Hindi-dubbed** shelf ("EN Hindi", "India EN
Dubbed") — never English; subtitled markets (`AR-SUBS`) say nothing about the audio.

**Stacking** (`vod_match::cluster_flat`, per kind; inputs: the stored hash, TMDB id, origin):
* Same key + same **known** year → one stack, **across languages and markets** (§29.1 #5).
  A missing year never stacks by key; years are never fuzzy (`Worth` 2020 / 2021 stay apart —
  the provider itself is inconsistent; 4 197 same-key pairs ≥ 2 years apart are remakes).
* Short (< 3), all-digit and stop-list titles (`It`, `1917`, `Live`) stack only with a year
  (which every key-stack has).
* **TMDB ids decide when present**: the same id stacks whatever the names or years
  (`Incendies` / `Die Frau die singt`); a same-key run with two ids splits by id, its id-less
  copies form their own stack; two different explicit origins split the same way
  (*The Office* (US) / (GB)).
* A copy the user split off leaves its stack for good; copies split off the same title stack
  together, as the other title (namesakes are separated in two steps, not one per copy;
  §29.11).
* The ratings check (report rule F) is dropped: provider ratings are too noisy.

Hash: `match_hash` = blake3 over (kind, key, year) with separators, cut to 64 bits (the
report's weak hash collided once on this data; this one doesn't by accident).

On the snapshot (every copy, before hiding): movies 301 965 → **163 481 cards (−45.9 %)**,
40 513 stacks (31 603 across playlists, 8 910 inside one; 29 793 hold two or more languages);
series 77 665 → **41 140 (−47.0 %)**, 11 786 stacks; 109 same-key series kept apart by two
origins; 42 834 movies and 3 052 series have no year (they stack only by TMDB id, which this
snapshot doesn't have yet). The app stacks only visible copies, so with the UK + English
filter the stacks are mostly English copies. Judged by eye at integration: 160 random stacks
(40 movie and 40 series stacks across playlists, 40 + 40 inside one) — no definite false stack,
one doubtful (a 2026 title in a Nordic and a Philippine category): ≈ 0–0.6 % of stacks. The
known false class is short namesakes of one year across industries (*Leo* 2023, *Gold* 2022):
only TMDB ids or "Not the same title" separate them.

### 29.11 P2: stacks in the library (`store::stacks`, `store::titles`)

**Data** (migration `0006_title_stacks`, idempotent — columns added by its Rust step only when
missing; it was 0005 before explicit content took that number, and a development database at
v5 with it gets `0005_explicit` applied on open, `migrations::reconcile_renumbered`):
* `movie` / `series`: `match_key` (folded key, diagnostics and P3 "Same title as…"),
  `match_hash` (NULL = can't stack by key), `match_lang` (`EN`, `AR-SUBS`, `FR+SUB`,
  `+MULTI`), `match_tags` (`VersionTags`; NULL = not computed), `match_origin`;
  `series.tmdb_id INTEGER` (movies keep `tmdb_id TEXT`).
* TMDB ids come from the movie and series **lists** (`tmdb` or `tmdb_id`, string or number;
  `""` / `"0"` = none) at import, and from `get_series_info` / `get_vod_info` details; details
  that bring a new id rebuild the library.
* `store::titles::refresh_source(conn, source)` computes the match columns per playlist (its
  frequent prefixes are part of the rules) at the end of every catalogue import
  (`finish_catalogue_with`) and once for every playlist in the migration's backfill. Only rows
  with `match_tags IS NULL` are parsed — the upsert clears it when the provider name or year
  changes — unless `meta` `vod_match:<id>` (rules version + prefix fingerprint) changed; only
  changed rows are written.
* `title_link(source_id, kind, item_key, action ∈ {split, pin, join}, at, fp, orphaned_since,
  join_group)` (`join` and `join_group` from migration 0007, P3 §29.16): the
  user's corrections, **user data** like a favourite (`store::user` HOLDERS): a refresh that
  re-keys the copy re-attaches them by fingerprint (else they are listed in Settings ▸
  Orphans), so a split never quietly comes undone. `titles::set_split` ("Not the same title",
  Undo = `false`), `titles::set_pin` (one pin per stack: pinning clears the other copies'),
  `titles::links` (Settings ▸ Hidden ▸ "Titles you separated, combined or pinned"). Write them with
  `DataScope::Overrides` (the library rebuilds); backups carry them.

**Build** (`stacks::build`, the last step of `library::load_with`, per kind): only copies in the
snapshot — hidden items (own flag, every category hidden, the category filter) are not, nor
**explicit content while parental control hides it**: `LoadOptions::visibility` is §20.3's
`Visibility`, the one predicate; the build leaves explicit copies out before stacking, so they
are never listed, counted, searched or stacked — and only from playlists in All
(`library::out_of_all`, shared with `ui::scope`). Locked copies stay (`VodSummary.locked`).
When the session relocks, `Library::hiding_explicit` re-filters the snapshot on the UI thread:
`VodIndex::without` re-points the stacks with `Stacks::retain` (members keep their order, a
stack left with one copy dissolves, the top is re-chosen) until the rebuild lands.
* Language of a copy: its tag; an untagged copy in a category whose tagged copies are ≥ 80 %
  one market (≥ 10) takes that market (`CopyMeta::inferred`, label `AR-SUBS?`) — it never
  passes as the user's language because the provider left the prefix off.
* Language rank (`vod_match::lang_rank`, user's `player.audio_langs` normalised `eng` → `en`):
  the user's languages in order, then untagged / multi-audio copies, then subtitled markets
  whose audio the tag doesn't say (`AR-SUBS`, `+SUB`), then other languages.
* Copy order inside a stack: language rank, CAM / TS after the clean copies of that language
  (whichever playlist has them), playlist order, the playlist's own order. **Top** = the first
  unlocked copy; all locked → the first.

**API** (`lib.vod.stacks(movies) -> &Stacks`; item = index into `vod.movies` / `vod.series`):
`stack_of(item) -> Option<StackId>`, `members(s)`, `top(s)`, `copies(item)`, `len()`,
`saved()` (All's card count = copies − saved; the scope bar's All counts cards), `retain(map,
&items)`, `elsewhere(item, &items)` ("+1 also in TV" inside one playlist), `meta(item) -> CopyMeta`
(dub, version tags, origin, TMDB id, rank, pin, split), `lang(item)`, `lang_label(item)`
(`EN`, `FR · sub`, `AR?`), `playlist_rank(source)`, `facts(item, &items) -> CopyFacts`.
Views of everything collapse lists with `ui::stacks::collapse` (§29.14) when
`interface.same_title = combine` (default; `show-each` = every copy is a card); one playlist
never collapses. `VodIndex` also caches name sort ranks per
snapshot (a name sort is an integer sort).

### 29.12 P2: which copy plays, marks (`stacks::play_order`, `stack_marks`)

`play_order(&[CopyFacts], session_unlocked, interface.copy_order)` → copy indices, first plays:
1. the pinned copy ("Always play from TV 2"); 2. the latest progress; 3. copies that failed
recently, whose account is at its connection limit or expired go last (`trouble`, the caller
knows); 4. language rank; 5. CAM / TS after the clean copies of that language — a CAM copy
in the first playlist never plays before a clean one in the second; 6. playlist order
(`copy-order = playlist-order`, default) — or best quality first (`best-quality`) — copies of
one playlist tie here; 7. mkv / mp4 before other containers (`container_ok`, unknown between). Locked copies are left out
unless the session is unlocked. `fallback(&order, failed)`: the next copy, tried **once** when
the first fails before its first frame (the player decides "once" and the toast).

`stack_marks(members, fav, watched, played_at, locked)`: ★ when any copy is a favourite, ✓ when
any is watched, progress from the latest played copy, the lock only when every copy is locked.
Writes from a stack card follow §3 of the recommendation: favourite / mark watched → the copy
that would play; unfavourite / unmark → every copy (with Undo).

### 29.13 P2 core: performance and tests

Measured (release): clustering the real 301 965 movies 10–16 ms, the whole stacking step
(`stacks::build`: languages, categories, clustering, order) **30 ms** (series 7 ms) — budget
60 ms; synthetic 300k (`benches/store.rs` `stacks/*`) in the same range. Parsing a name ≈ 1.6 µs
(import / migration only: 0.5 s for 302k). No per-frame work: stacks are built with the
snapshot; `collapse` runs when a list is rebuilt. `benches/startup.rs` and `benches/store.rs`
seed paired copies keyed like an import, so the library-load gate includes stacking. At
integration (after explicit content): library load 100k channels + 50k movies + 20k series
**309 ms** (origin/main, whose bench seeds no copies: 277 ms, same machine back to back) —
budget 400 ms; `stacks/build_300k_movies` 42 ms, `stacks/cluster_300k_movies` 12 ms; on the
real snapshot 29 ms / 10 ms (series 7 / 3 ms).

Tests: `vod_match` (the report's provider spellings per rule, false-merge cases as tables —
namesakes across industries, remakes by origin, TMDB override, missing / near years, generic
titles; dub / sub / version tags; frequent prefixes; language ranks), `stacks` (top card with
locked / CAM / foreign copies, in-All and split, category-inferred markets, play order and
fallback, CAM across playlists, `retain` after a removal, split copies of one title together,
stack marks), `store_catalogue` (import keys and TMDB ids, hidden vs locked vs out of All vs
explicit content at build and on relock, split surviving a refresh and its Undo, a split
re-attached after the provider re-keys the copy, one pin per stack, a renamed copy re-keyed),
the 0006 migration (backfill, re-run, a database that ran it as 0005), Xtream list / info TMDB
parsing.

### 29.14 P2 UI: stack cards, Copies, Play from (`ui::stacks`, `screens::copies`)

**Where copies combine.** Only views of everything combine, and only with Settings ▸ Interface
▸ "Same title in several playlists" on **Combine** (`interface.same_title`, default; "Show each
copy" turns every copy back into its own card everywhere, no rebuild):

| View | One card per stack | Notes |
|---|---|---|
| Movies / Series, scope All | All, every category, Continue watching, Favourites | `ui::stacks::collapse`: the stack's place is where its first copy comes in the list (the sort holds); the card is the stack's **top** when the list has it, else the list's own copy (unlocked first) — a playlist's category shows its own copy, Favourites the favourite one |
| Movies / Series, one playlist | never | a title also in other playlists gets one card edge and `+1` on the meta line (tooltip "Also in TV"); playlists out of All have no stacks, so no `+1` |
| Home | Recently added, Recently updated series (renamed: it sorts on the provider's date), favourites rails, Continue watching | top copy; the copy played last; the first favourite copy |
| Search | Movies and Series hits | the best-matching copy, a code chip per playlist of the stack (`[TV] [TV2]`; none with one playlist); Enter opens the stack's top card's page |

**Stack card** (`widgets::poster`, `TileDecor.stack`): two card edges offset down-right behind the
poster (the card keeps its footprint), a copy-count badge bottom-right above the progress bar,
and its playlists' code chips top-left (`[TV] [TV2] [NW] +1`: up to three, fewer on a narrow
card, the meta line stays `2021 · ★ 7.1`). "Playlist on cards: Never" drops the chips, not the
edges or the count. ★ / ✓ / progress / lock follow `stacks::stack_marks`: progress of the copy played last,
the lock (and the disabled look: dimmed poster and card edges, muted title) only when every
copy is locked and the session is locked. Fanned out, the head card names its own copy (`TV ·
EN · 4K` on the meta line, its own chip only); the accent badge still marks the stack. Accessible label:
`Dune · TV + TV 2 · 3 copies[ · locked]`. With a stack card focused the hint line adds "Space
copies · Ctrl+O play from" (`Screen::hint_extras`, keys from the context's tables).

**Keys and mouse in the grid** (`[PosterGrid]`):

| Input | Stack card | A copy shown fanned out |
|---|---|---|
| Enter / click | the page of the card's copy (Copies section); that copy locked → PIN | its own page, which then plays that copy (`ui::stacks::open_copy`) |
| Shift+Enter | plays the copy that plays first with its fallback (`ui::copies::play_title`; the resume prompt first when any copy has progress); every copy locked → PIN | plays that copy |
| **Space** / click the count badge | fans the copies out right after the card, in copy order, each with its language and version (`TV 2 · EN · 4K`; locked ones dimmed) | folds its stack |
| Esc | folds every open stack first (after the category marks, before the drawer) | same |
| Ctrl+O | "Play from…": every copy (`TV · EN · 4K`), "plays first" on the one that would; a locked one asks for the PIN | same |
| Ctrl+F | no favourite copy: adds the copy that would play; else every copy leaves Favourites (toast with **Undo**) | that copy |
| Ctrl+W | marks the copy that would play | that copy |
| Ctrl+L / Ctrl+H | locks or unlocks every copy (the PIN first) / hides every copy, the toast naming their playlists — both with Undo | that copy |
| F2 | "Rename every copy": copies with a name of their own keep it unless Enter is pressed again after the dialog says so; Undo | that copy |
| Delete (Continue watching / Favourites) | forgets every copy's progress / takes every copy out, with Undo (the rows as they were) | that copy |
| menu | Play from…, Show each copy / Fold, … | adds "Not the same title" |

Space was free in `[PosterGrid]` (the group lists' Space marks categories in `[List]`, a
different pane), so the decision of §29.1 #4 holds; it is bound as the `Text(" ")` event like the
marks, so while the grid is filtered it types a space instead (fold with Esc, or clear the
filter). Ctrl+O is bound in `[PosterGrid]`, `[PosterGridFiltering]`, `[Detail]` and
`[PlayerVod]` (§29.15), one text everywhere ("Play from another copy…"); Ctrl+Shift+O stays the
pop-out. `Action::Undo(n)` is never bound: it is a toast's "Undo" button, handled by the app
(§29.15), so it reverts its own action whatever screen shows. Home tiles and Search rows of a
stack take the same combined ★ / lock / hide / rename (Search changes locks only in an
unlocked session).

**Detail pages** (`screens::copies::Copies`, shared by the movie and series pages): with two or
more copies, a **Copies · N** section of mini-cards (`widgets::copy_card`) after the buttons,
one per copy in copy order — playlist glyph + name, language and version (`EN · 4K HDR`,
`AR-SUBS?` for a market inferred from its category), progress bar, status at the right:
*plays first* / *always plays first* (pinned) / *locked* (dimmed while the session is locked) /
*failed 2 h ago* (the playback hook below). Under them: "Matched by TMDB id" or "Matched by
title and year (2021) · Not the same title? Shift+F10 on a copy".
* The copies and their state are `store::copies::plan` (§29.15), read off the frame budget
  when the library, progress, overrides, the session's lock or the copy-order setting change;
  the mini-card status is `plan.status` ("failed 2 h ago", "playlist at its connection limit",
  "account expired", "locked"), progress from each copy's own row.
* **Which copy Play uses**: the one picked on the page (click, Enter on a focused mini-card,
  "Play from…"; a locked one after the PIN), or the copy opened on purpose (a fanned-out copy,
  one playlist's list), else **the title's first copy in play order** — whichever card led to
  the page (a category, Favourites, Home, Search), so Play and Shift+Enter on the card agree
  and a pin is honoured. Play resumes the **latest progress of any copy** ("Resume 41:20 ·
  TV", `movie_command`'s `start_at`), with the next copy as its fallback; a series page plays
  its episodes with the same episode of the next copy as the fallback, and a series' other
  copy plays its own next episode.
* "Play from ▾" button and Ctrl+O: the copies menu as in the grid.
* A mini-card's menu (right-click, the menu key): Play from TV 2 · Always play from TV 2 /
  Don't always play from it (`titles::set_pin`, one pin per stack) · Not the same title
  (`titles::set_split`, toast with Undo) · Open its page (series). The page's ⋯ menu has Play
  from… and "Not the same title" for the page's own copy.
* Focus: the movie page's ←/→ walk the buttons then the mini-cards; the series page has a
  Copies section between the buttons and the season tabs (↑/↓, Tab).
* A page whose copy left its stack because its details brought a different TMDB id says
  "Shown on its own now: its details name another title than its other copy".

**Settings** (only with two or more playlists of movies or series). Interface ▸ *Several
playlists*: "Same title in several playlists" (Combine / Show each copy) and "Which copy plays
first" (Playlist order / Best quality — `interface.copy_order`; language, CAM, pin, progress
and trouble still come first). Hidden ▸ "Titles you separated, combined or pinned" lists `titles::links`
(split: "TV 2 · shown on its own · Enter combines it again"; pin, with the pin glyph: "TV ·
always plays first · Enter undoes"); Enter or the row's menu undoes. It shows for one
playlist too while it has rows.

**Cost.** Collapsing runs when a list is rebuilt (category, sort, scope, library, setting or
open stacks change): O(list + its stacks' copies), with a set instead of a flag per title for
short lists (Favourites is recomputed every frame). Per frame only visible cards look at their
stack.

**Tests.** `ui::stacks` (card choice in All / a category / Favourites, fan-out order, card
facts, labels, Show each copy), Movies (stack card label and place, the hint line, Space /
badge / Esc, the fanned head card's copy, Ctrl+O and its pick, Show each copy, All's card
count, one playlist with `+1`; locked copies kept and drawn disabled, all-locked card asks for
the PIN, a hidden copy and an explicit copy leave the stack; Favourites / Continue watching
across copies with the app's Undo; hide / lock / rename covering every copy; a narrow card's
"+1" and badge), the movie page (mini-cards, pick, another copy's page playing the first copy,
a copy opened on purpose playing itself, Ctrl+O, pin, split with Undo, a locked copy picked
after the PIN), the series page (Copies section, the other copy's next episode, its page),
Home (one tile per stack in every rail, Ctrl+F on a stack, Show each copy), Search (one row
with both playlists), Settings (split / pin list with Undo, the two choices), the rename
dialog (asks before overwriting a copy's own name), keymap (Space and Ctrl+O per context).

### 29.15 P2: playing a title's copies, and actions on them (`store::copies`, `ui::copies`)

**Which copy plays** — `copies::plan(conn, &vod, movies, item, session_unlocked, copy_order,
now) -> CopyPlan` reads, per copy of the title (a handful of indexed rows: progress, the
`stream_failure` row, the container, the playlist's account), the facts §29.12's
`play_order` needs and returns every copy in copy order (`copies`, locked ones included, for
the detail page's mini-cards) plus the play order (`order`; `first()`, `fallback()`,
`instead_of(failed)`, `resume()`, `status(item)` = plays first / locked / failed at /
at its connection limit / account expired). Trouble: the copy's own failure in the last 24 h;
its playlist refused a stream (401/403) in the last 10 min — at its connection limit; its
account expired, banned or disabled. A series copy's progress and failures are those of its
episodes. Called when a detail page opens, Play is pressed or a stream failed — never per
frame. A series page continues from the copy with the newest episode progress
(`copies::next_episode`); `copies::same_episode` finds an episode in another copy (loaded
episodes only).

**Fallback, once** — `PlayerCommand::PlayCopy(CopyPlay { req, fallback, start_at })`
(`ui::copies::movie_command` / `episode_command` build it from a plan). The controller arms
the machine (`PlayerMachine::arm_fallback`) only for a target that resolved. An armed VOD
target that fails before its first frame gives up sooner (a load timeout at once, other
retryable failures after one quick retry; the 403 retry stays) and emits `Effect::Fallback`
instead of Error: the failure is recorded, the controller resolves the other copy, starts it at
the position the first was asked to start at and sends `PlayerEvent::FellBack` → the toast
"TV couldn't play Dune — playing from TV 2". A first copy that can't even be resolved goes to
the fallback directly. When the next copy is in the first copy's playlist,
`CopyPlay::elsewhere` names the first copy of another playlist, tried instead when the failure
was the playlist's (HTTP 401 / 403, our connection budget for it). The machine is disarmed after
one fallback; if the fallback can't be resolved, `Input::GiveUp` puts the first failure on
screen. The resume prompt carries the copy play (`ResumeSpec::copy`), so "Resume 41:20" from a
card keeps the fallback and the latest progress of any copy. After the first frame nothing
switches by itself: the normal reconnects run, then the error card offers the other copy
("Play from TV 2 at 41:20" — "Play from TV · 4K at 41:20" for another copy of the same
playlist; button or `Ctrl+O` = `Action::PlayFrom`; `ui::copies::other_copy` looks it up once
per failure within the frame budget; locked copies only for an unlocked session) from
`PlayerSnapshot::last_position` (kept across reconnect reloads). During playback Ctrl+O lists
every other copy (`ui::copies::other_copies`) and plays the one picked from where playback is. `start_at`
overrides the copy's own progress; "Resume 41:20" on a combined card continues the latest
progress whichever copy plays (a pin or a pick).

**Actions on a combined title** (recommendation §3; data stays per copy):

| Action | Writes (`store::copies`) |
|---|---|
| ★ add / remove | the copy that would play (nothing when a copy already is one) / every copy (`favourite`) |
| ✓ mark / unmark, forget | the copy that would play / every copy's progress (`watched`; `forget_series`: every copy's episodes) |
| Lock, Hide | every copy (`lock`, `hide`; the hide toast names the playlists) |
| Rename | copies without their own rename; `rename_conflicts` lists the others, `overwrite` takes them too |
| Not the same title, Always play from | `split`, `pin` (one pin per title) over `titles::set_split` / `set_pin` |

Every action returns an `Undo`: the rows of its table (`fav_item` of the list, `watch_progress`,
`item_override`, `title_link`) for those copies as they were; `copies::undo` deletes the
copies' rows and inserts these back (positions, times, fingerprints included). `ui::copies`
runs the write on the store writer and, once it succeeded, stashes the rows in the app's
`Services::undo` (`copies::UndoStash`, the last 8) and toasts "… · Undo" with
`Action::Undo(n)`; the app writes action `n`'s rows back with the right `DataScope` — whatever
screen shows and whatever was done since. Inside one playlist the screens keep `kit`'s one-copy
actions.

### 29.16 P3: copies everywhere else (`store::copies`, `ui::copies`, `overlays::{same_title,delete_playlist}`)

P3 finishes recommendation §7's last phase. (Home and Search combined, and the "Recently
updated series" rename, shipped with P2.)

**Detail pages act on every copy.** A movie or series page whose title has several copies
(`screens::copies::Copies::combined`: a stack while copies combine, and the page wasn't opened
for one copy on purpose — a fanned-out copy or one playlist's list, `ui::stacks::open_copy`)
takes the stack card's rules (§29.14, §29.15): ★ shows when any copy is a favourite; adding
puts **the copy Play would use** (the one picked on the page, else the first in play order) in
Favourites, removing takes every copy out; ✓ (movies) shows when any copy is watched, marking
marks the copy Play would use, unmarking forgets every copy's progress; Ctrl+L / Ctrl+H / F2
lock, hide or rename every copy (`ui::copies::{toggle_lock, hide, rename_dialog}`, shared with
the grid, Home and Search). Each write has its Undo toast, naming the playlists when it covers
every copy ("Removed from Favourites in TV and TV 2", "Marked unwatched in …", "Locked Dune in
…"; a Continue-watching card's Remove keeps "Removed from Continue watching"); the menu says
"… every copy". A page
opened for one copy on purpose keeps acting on that copy, like the card that led there.

**Series: episodes matched across copies.** A combined series page reads, with its own
episodes, the progress of the other copies' episodes by (season, episode)
(`copies::episode_progress`: a watched mark wins, else the latest progress) and shows it merged:
an episode watched in TV 2's copy shows ✓ here, "Mark season watched" says *Season watched*.
The page opens at the next episode of **the copy with the newest progress**
(`copies::next_episode`, the same rule Play and Shift+Enter on a card use — verified), as this
copy's episode. ✓ on an episode and "Mark season watched / unwatched" write the same episodes
of **every copy** in one write with Undo (`copies::mark_episodes`; a copy without that episode
is skipped; "Marked S01 watched in TV and TV 2"). Xtream series load episodes lazily: when
another copy's aren't loaded (`copies::unloaded_series`) the page writes this copy and the
loaded ones **at once** (nothing is lost if the user leaves or quits) and submits
`LoadSeriesDetails` for the others (`ui::copies::EpisodeFetch`: polls the job list and the
database at most four times a second), shows "Loading the other copy's episodes to mark them
too…" with a spinner, then writes them with a second Undo toast. Each waiting mark carries the
copies it was made for (a list, in order: two marks during one load both land; leaving for
another series never redirects them); a copy that couldn't load is named in a warning ("TV 2
couldn't load its episodes — marked in the other copies only"). Play from a combined page
resumes from **the newest progress of that episode in any copy**: the button says "Resume
S01E04 12:00" and `ui::copies::play_episode` prompts at that position and starts this copy there
(`CopyPlay::start_at`), as movies do.

**"Same title as…".** The user combines a title with another the matcher kept apart (another
name, a missing year, two TMDB ids): a card's menu (Movies / Series grid) or a detail page's ⋯
menu (while copies combine) opens `overlays::same_title` — type to find the other title (one
row per title: a stack's top, never the title's own copies; ≤ 200 matches, name · year ·
playlists · copies), Enter or a click combines them (`copies::join` → `titles::join`, Undo toast
"Dune and Die Frau die singt are one title now"). Stored as `title_link` rows with action
`join` for both copies and a shared `join_group` (migration `0007_title_join` rebuilds the table
for the new CHECK in its Rust step, which copies `join_group` too on a re-run); joining a copy
already joined merges the groups; a group left with one copy (its partner's playlist deleted,
a backup merge meeting joins already here) is dropped (`titles::prune_joins`). The menu item
shows with two playlists of that kind or more; the toast adds "in All playlists" when one
playlist is in scope (nothing changes there). The build
(`stacks::build` → `apply_joins`) stacks a group's copies **with the stacks they are in**, after
clustering and on top of splits (a split copy can still be joined elsewhere), only in All; the
copy order inside is the usual one. Like splits and pins the rows carry fingerprints, so a
refresh that re-keys a copy re-attaches them; backups carry the group (renumbered after the
groups already there on a merge restore). The page says "Combined by you (Same title as…)";
Settings ▸ Hidden ▸ **"Titles you separated, combined or pinned"** lists each joined copy
("TV · combined with Die Frau die singt (TV 2) · Enter separates it"; an explicit partner isn't
named while explicit content is hidden); Enter takes that copy out
(`titles::unjoin`; a group left with one copy goes too).

**Deleting a playlist.** Settings ▸ Playlists ▸ Delete playlist… opens
`overlays::delete_playlist` instead of a plain confirm. It reads `copies::move_plan(source)`
(off the UI thread) and, when the playlist has favourites or progress, **offers — unticked —**
"Move favourites, progress and watched marks to the same titles in other playlists", listing
what would move ("Dune (2021) → TV 2 · ★ · 41:20"; "… → TV 2: Dune: Part One (2021)" when the
copy reads differently) and what can't ("Gold (2022) — no copy in another playlist", "3 channel
favourites — channels aren't matched across playlists"); titles concealed while explicit content
is hidden move like the others but are only counted ("And 2 hidden titles"). The subtitle says
"This can't be undone."; a plan that can't be read says so ("Delete removes everything"). The
focus (box → Cancel → Delete) stops at the ends — ↑ never wraps onto Delete. Matching: the
other copy with the **same TMDB id**, or the **same match key and known year** (`match_hash`,
never against a different TMDB id or another explicit origin, `match_origin`), or **joined by
the user**; never a copy either side split off; the first in the playlist order (enabled
first). The other playlists' titles are read once per kind (one scan, indexed in memory) and a
series' copy once for all its episodes, so a big catalogue doesn't make Delete wait. Episode progress moves to the same season and episode of
the series' other copy **only once that copy's episodes are loaded**: ticking the box starts
`EpisodeFetch` for them ("Loading the other copies' episodes (1 of 2 series)…"; Delete waits),
then the plan is read again; episodes whose copy couldn't load are listed as not moving. Delete
runs one writer job: `copies::move_titles` (a favourite joins the same list at the same place
unless the copy is already there; progress — read when Delete runs, so playback saved since
the dialog opened counts — replaces the copy's own only when newer) then
`sources::delete_source`; the toast says "Deleted TV — moved 3 favourites and 2 progress marks
to the other playlists; 2 couldn't move". **No Undo**: the playlist and its catalogue are gone
(feasible only for the moved rows, which are ordinary data of the other copies). The old
confirm's "kept as orphans" was wrong — the rows cascade with the playlist — and is gone.

**Live TV: "Also on ● TV 2 (FHD)".** The preview pane lists the cursor channel in other
playlists (`live::also_on`): the same `family` (normalised cleaned name, quality words dropped),
another playlist, not hidden (`LiveIndex::hidden`), not DRM-only; explicit channels hidden by
parental control aren't in the snapshot; a channel whose **tvg-id differs** from the cursor
channel's (both known; one indexed read in the frame budget for the chips — Ctrl+O, a key press,
reads it at once) is another channel. Best quality
first within a playlist, playlists in their order, ≤ 12. Each is a chip in its playlist's colour
— `TV 2 (FHD)`, plus the alternative's name when it differs, the lock when locked, ▶ on the one
playing — that plays
it here on a click (`widgets::badge::source_chip_button`); **Ctrl+O** in the channel list
(bound in `[List]` / `[ListFiltering]`, "Play from another playlist (Also on)…") opens them as
a menu "Channel 0 is also on". A locked alternative asks for the PIN (`Unlocked(PlayFrom)`).
Never merged, never switched to by itself; the cursor stays where it was. The hint line adds
"Ctrl+O also on" while the cursor channel has alternatives.

**Chores.** `store::vod::recently_added` (dead since Home moved to the library) is removed.
Ctrl+O during normal playback (the player's "Play from" menu) has its UI test. The AR-DE ranking
note (§29.11's language ranks) stays as it is.

**Tests** (real behaviour): the movie page's combined ★ / ✓ / rename / lock / hide with Undo and
a copy opened on purpose (`movie_detail`); series marks across copies, next episode from the
newest-progress copy, a season marked in every copy with Undo, a lazily loaded copy fetched
first and a copy that can't load named (`series_detail`); joins in the build — whole stacks
merge, out of All ignored, a split copy joined elsewhere (`store::stacks`); a join stacking two
TMDB ids, surviving a refresh and a re-keyed copy, merging groups, separating
(`store_catalogue`); the 0007 migration (rows and fingerprints kept, joins accepted, re-run,
cascade); the picker (own copies never offered, Enter joins, the library stacks, Undo); the
Hidden list's join rows and Enter; backups carrying a join; the move plan (TMDB / key + year /
joined matches, a different TMDB id and a split copy refused, channels and missing copies
listed, episodes waiting for the other copy's episodes) and the move itself (list places,
newer progress kept) (`store_catalogue`); the delete dialog (unticked by default, listed, moved
when ticked, nothing moved unticked, episodes loaded first) (`overlays::delete_playlist`); Live's
"Also on" (same name, another tvg-id refused, hidden refused, click plays, Ctrl+O menu, PIN for
a locked one, explicit refused while hidden, Ctrl+O with the frame budget spent) (`live`);
Ctrl+O during playback (`player`); keymap (Ctrl+O in lists). Review fixes add: marks made
during a load land in order and never on another series, this copy written at once
(`series_detail`); resume from the other copy's position (`series_detail`); the dialog's focus
never wraps onto Delete and explicit titles counted, not named (`overlays::delete_playlist`);
an explicit join partner unnamed in Settings ▸ Hidden (`settings`); origins refused, progress
read at Delete, a join of one dropped (`store_catalogue`); 0007's re-run keeps `join_group`
(`migrations`).

---

## 30. Agent control (MCP): use Tuner from an AI agent

"Touring cars is on" → the agent finds the race in the guide and Tuner switches to it;
"something light under 2 hours" → the agent browses the library, picks a film and starts it.
The user prompts an AI agent (Claude Code, Codex, Gemini CLI, Cursor, VS Code with Copilot, or
any other MCP client); the agent talks to Tuner through an MCP server that the `tuner` binary
itself provides. Tuner names the agent on its own screens by the name its client reports
(§30.12), and connects the agents it finds from its own first-start card or Settings ▸ AI agents
(§39).

### 30.1 Decisions
1. **`tuner mcp`** is an MCP server over stdio for any MCP client (`claude mcp add tuner --
   tuner mcp`, `codex mcp add tuner -- tuner mcp`, or a `command: tuner, args: [mcp]` entry in
   the client's config). It is a thin bridge: it never opens the database. Every answer comes
   from the **running** Tuner over the existing IPC socket, so it uses the app's in-memory
   library and guide and honours every visibility rule the screens honour.
2. **Hand-rolled protocol, no `rmcp`.** Checked on crates.io (2026-10-11): `rmcp` 3.5.1 (the
   official SDK, MSRV 1.88; pinnable at most to 3.4.1 under the two-week rule) is mature but
   would add `schemars` 1, `chrono`, `uuid`, `rmcp-macros`/`pastey`, `indexmap` and the
   `tokio/io-std` feature to a desktop binary, and has had three major versions in a year. We
   need five methods of a JSON-RPC protocol that has been stable since 2024-11-05
   (`initialize`, `notifications/initialized`, `ping`, `tools/list`, `tools/call`), and want
   hand-written schemas and descriptions anyway. ~300 lines of `serde_json`, no `Cargo.toml`
   change (frozen, and three branches are in flight). Revisit if we ever need resources,
   prompts, sampling or the HTTP transport.
3. **IPC v2** on the same socket: a versioned request/response line next to the v1 handoff.
   Same-uid peers only (`SO_PEERCRED`), 64 KiB / 5 s per connection, unchanged.
4. **Answers off the UI thread.** The UI thread may not run while the window sits on a hidden
   workspace (§11.1), so the handler runs on the tokio runtime with the library snapshot the app
   publishes, its own store readers, the player handle and the parental state computed at
   request time. Only `play` needs the UI (PIN prompt, zap list, resume rules): the handler
   validates, then sends an `AppEvent` and raises the window.
5. **Launch if needed.** Every tool except `status` and `control` starts Tuner when it isn't
   running (detached) and waits for it; `status` says `running: false`, `control` says it isn't
   running.
6. **Auto-switch** is a reminder whose action is "switch": at the programme's start Tuner plays
   the channel by itself, after a "Switching in 10 s · Cancel" toast. No migration: the marker
   lives in `reminder.lead_s` (`Reminder::SWITCH_LEAD` = −1; older builds read it as "default
   lead" and just notify). `user_version` is positional and `feat/p3-copies` holds 0007.
7. **Guardrails first** (§30.6): nothing parental control hides, ever, in any field or count;
   locked items are flagged and the PIN is only ever typed into Tuner; no URLs, logins or
   provider internals in any result; actions logged by ref, not title.

### 30.2 `tuner mcp` (`agent::mcp`, `agent::tools`, `agent::client`)
* `main` dispatches `Cli::is_mcp()` right after resolving the paths (`tuner mcp`, and a lone
  `mcp` word after global flags — `tuner --debug mcp`, which clap reads as an input file): no
  config load, no instance lock, no `logging::init` (that writes `tuner.log`, which the running
  app owns). Logging goes to stderr only (WARN; DEBUG with `--debug`) — MCP clients such as
  Claude Code keep a server's stderr in their log. stdout carries protocol lines only.
  `--profile-dir` selects the profile's socket (tests, second profiles) and is passed on when
  launching.
* JSON-RPC 2.0, one message per line. `initialize`: `protocolVersion` = the client's if it is in
  `tools::PROTOCOL_VERSIONS` (2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05), else the newest;
  `capabilities: {tools: {listChanged: false}}`; `serverInfo: {name: "tuner", title: "Tuner",
  version}`; `instructions` = `tools::INSTRUCTIONS` (the workflow: guide_search → play /
  schedule_switch; categories → browse → details → play; refs are opaque; the PIN rule).
  `clientInfo` (`name`, optional `title`, each cut to 64 chars) is kept for the session and
  sent with every request (§30.3, §30.12); a second `initialize` replaces it.
  Notifications are ignored; `ping` → `{}`; unknown method → −32601; unparsable → −32700; a
  line over 1 MiB → −32600 (not buffered whole). Requests are handled in order.
* `tools/list` → `tools::tools()` (name, title, description, `inputSchema`, `annotations`:
  `readOnlyHint`, `destructiveHint: false`, `idempotentHint`, `openWorldHint: false`). No
  `outputSchema` (results are documented below; `structuredContent` is sent regardless).
* `tools/call` → `proto::Call::from_tool(name, arguments)`: the arguments are the IPC params,
  unchanged. A schema mismatch or an IPC error is a **tool error result** (`isError: true`,
  text `"<code>: <message>"`), not a JSON-RPC error, so the model can correct itself. Success:
  `{content: [{type: "text", text: <compact result JSON>}], structuredContent: <result>,
  isError: false}`.
* `client::Client::call(call, launch)`: one connection per request, 6 s timeout, reply ≤ 1 MiB.
  No socket / refused + `launch` → `Launcher::launch` (production `ExeLauncher`: this executable,
  `--profile-dir` if given, own process group, stdio `/dev/null`, so it survives the agent
  and never writes into the protocol stream), then poll the socket every 100 ms for ≤ 15 s;
  `ErrorCode::Loading` replies are retried every 250 ms for ≤ 20 s. A reply without `"v"` (a
  pre-v2 Tuner answering `bad request`) → "restart Tuner to update it". Status without Tuner →
  `{"running": false}` as a normal result.
* As built (bridge track): an unknown tool name is JSON-RPC −32602 (the spec's example), bad
  arguments stay a tool error. Client failures are tool errors with their own codes
  (`not_running`, `did_not_start`, `loading`, `too_old`, `connection_failed`). A result after a
  launch carries a second text block "Tuner wasn't running, so it was started."; `status` from a
  running Tuner gets `running: true`. The bridge also withholds any result containing `"://"`
  (`internal`), behind the server's own check. `Launcher::failed` lets the wait end early: the
  `ExeLauncher` reaps its child on a thread and a non-zero exit before the socket appears is
  `did_not_start` with the exit status, at once (exit 0 = it handed off to an instance starting
  at the same time: keep waiting). After a package upgrade `/proc/self/exe` ends in
  " (deleted)": the launcher runs the new file at that path. `--profile-dir` is made absolute
  before it is passed on.

### 30.3 IPC v2 protocol (`agent::proto`, implemented in the skeleton)
```text
→ {"v":2,"id":7,"method":"guide_search","params":{"query":"touring car"}}
← {"v":2,"id":7,"ok":true,"result":{"programmes":[…],"truncated":false}}
← {"v":2,"id":7,"ok":false,"error":{"code":"not_started","message":"…"}}
```
* One line per connection, newline-terminated. `ipc.rs` peeks `"v"`: absent → v1
  `IpcRequest` (unchanged); 2 → `AgentService::handle`; other → `unsupported`. v2 requests never
  send `AppEvent::Ipc` and never raise the window by themselves.
* `params` use `deny_unknown_fields`; every numeric limit is clamped by the handler; free text ≤
  200 chars (`proto::MAX_TEXT`).
* An optional top-level `"agent":{"name":"codex-mcp-client","title":"Codex"}` carries the MCP
  client's `clientInfo` (§30.12). It sits beside `method`, not in `params` (those are the tool
  arguments). Additive, so `v` stays 2: an older app ignores the key (`Request` has no
  `deny_unknown_fields`), an older bridge omits it (the agent is then unknown), and a request
  without it serialises as before.
* Error codes: `bad_request`, `not_found` (also for anything concealed — indistinguishable on
  purpose), `unavailable`, `not_started`, `loading`, `rate_limited`, `busy`, `unsupported`,
  `internal`.
* **Refs** (`proto::AgentRef`): `channel:2:xt:live:1234`, `movie:3:m3u:0123…`,
  `series:2:xt:series:9`, `episode:2:xt:ep:77` (`<kind>:<source>:<item key>`: M3U movie and
  channel keys look alike, so the kind is part of the ref); `programme:2:xt:live:1234@<start>`
  (the channel and the programme's start as shown on it, tvg-shift applied — stable across EPG
  re-imports, unlike `programme.id`). A bare `2:xt:live:1234` (`--play`) reads as a channel.
* **Times**: results `2026-10-11T21:00+01:00` (local, offset); params accept `now`, `HH:MM`
  (today), local `YYYY-MM-DDTHH:MM`, RFC 3339, unix seconds (`proto::parse_when`).

### 30.4 The app side (`agent::server`, `agent::shared`, `app.rs`)
* `Services::agent: Arc<AgentShared>`; `start_integrations` builds the `AgentHandler` (shared,
  store, config, player handle, bus, jobs) and starts the IPC server with it.
* The app publishes `AgentShared::publish_library(&self.lib)` wherever `TunerApp::lib` is
  replaced (one helper; the returned old snapshot goes through `release`, never dropped on the
  UI thread) and `set_unlocked_at(parental.unlocked_at())` after PIN entry / lock (every frame in
  `sync_explicit` is fine — it's a mutex store).
* `AgentShared::parental(cfg)` → `ParentalView { unlocked, hides_explicit, pin_active }` from the
  published unlock instant and the live config: a session that relocked by timer while the UI
  slept counts as locked.
* Readers: a small pool (≤ 3) of lazily opened store readers behind a mutex; every query runs on
  `spawn_blocking` inside `HANDLER_BUDGET` (4 s).
* `play`: resolve the ref against the snapshot (fallback: the DB like `channel_for`), apply the
  guards, compute the outcome (`pin_required` when locked and `!unlocked`), resolve where it
  shows (`act::agent_show`, §30.13), send `AppEvent::Agent(AgentCommand::Play(AgentPlay {
  target, from_start, place }))`, call `hypr::focus_self()` only when it shows in the window
  (`AgentShow::raise`), reply with `opened_in`. The app's `on_agent_play` plays exactly like
  the UI, then moves the window to `place` (`agent_place`): channels via
  `play_channel(id, PlayOrigin::Cli)` (PIN prompt, zap list, fullscreen); catch-up like
  `live::play_catchup`; movies / episodes with `PlayRequest { resume: !from_start }` (copies:
  `ui::copies` play order and fallback, as Enter on a card); series → next episode over its
  copies (`store::copies::next_episode`).
* `control` → `PlayerCommand::{SetPause, TogglePause, Stop, SetVolume, ToggleMute}` (mute /
  unmute only toggle when needed); `status` ← `PlayerHandle::snapshot()` + pending switches.

### 30.5 Tools (`agent::tools` — schemas there are the contract; params = `proto` structs)
| Tool | Params | Result | Notes |
|---|---|---|---|
| `guide_search` | `query`, `from?`, `to?`, `include_past?` (true), `limit?` (20, ≤ 50) | `{programmes: [ProgrammeOut], truncated}` | `store::epg::search` (FTS; live, upcoming, past order) fetched wider (≤ 200) then time-filtered; one row per programme with the best visible channel (favourite, unlocked, best quality) and `also_on`. |
| `whats_on` | `at?` (now), `channel?`, `group?`, `favourites_only?`, `limit?` (30, ≤ 100) | `{at, programmes, truncated}` | Channels with a guide, favourites → recently watched → display order, scan capped at 1 500 channels (`truncated`); `channel` matches name words or number. |
| `search` | `query`, `kinds?`, `limit?` (20, ≤ 50) | `{results: [ItemOut]}` | `nucleo-matcher` over the snapshot's visible names (like `search::collect_items`); one row per stack. Not scoped (like the Search card). |
| `details` | `ref` | `DetailsOut` | Movie/series: lazy `LoadMovieDetails` / `LoadSeriesDetails` when not loaded, waits ≤ 3 s, else `details_pending`. Channel: now/next/5 upcoming. Programme: description, categories, catch-up. Locked + session locked → name + `locked` only. |
| `categories` | `kind`, `playlist?` | `{categories: [CategoryOut]}` | Visible groups (not hidden, non-empty), All-scope playlists (`in_all`). |
| `browse` | `kind`, `group?`, `playlist?`, `unwatched?`, `max_minutes?`, `min_rating?`, `year_from?`, `year_to?`, `sort?`, `limit?`, `offset?` | `{items: [ItemOut], total}` | All scope (`ScopeFilter` for All: `in_all` off excluded unless nothing is left) or the named playlist; stacks collapsed; unknown runtimes after known ones under `max_minutes`. |
| `continue_watching` | `limit?` (10, ≤ 30) | `{items: [ContinueItem]}` | `store::user::continue_watching`, every playlist (a user list), `Library::conceals` + live parental check. |
| `favourites` | `kind?`, `list?`, `limit?` (50, ≤ 100) | `{lists: [FavListOut]}` | `load_favs` on a handler reader; via `by_ref` / VOD lookups, so hidden and concealed items drop out. |
| `status` | — | `StatusOut` (+ `running: false` from the bridge) | Never launches. |
| `play` | `ref`, `from_start?`, `open_in?` (`popout` / `full_window` / `where_i_am`) | `PlayOut {outcome: playing / catch_up / pin_required, ref, title, resume_at_min?, message, opened_in: popout / window}` | Programme: live → channel (with `from_start` + catch-up → restart from the archive); past → catch-up if `in_window` else `unavailable`; upcoming → `not_started`. Where it opens: §30.13. |
| `schedule_switch` | `ref` (programme, or channel + `at`) | `SwitchOut {id, channel, title?, start, warn_at}` | Not for something already started (`unavailable`: use play); ≤ 20 pending switches; an existing reminder of that programme becomes a switch. |
| `cancel_switch` | `id` | `{cancelled}` | Only switch reminders. |
| `control` | `action` (pause, resume, toggle_pause, stop, volume, mute, unmute, previous_channel, popout, window), `value?`, `tile?` | `StatusOut` | Never launches; volume clamped 0–130; `window` moves what plays to the window, fullscreen and raised (§30.13). |

Example flows: "touring cars is on" → `guide_search{query:"touring car"}` → a `live` hit →
`play{ref}`; an `upcoming` one → "It starts at 13:15 on ITV4 — want me to switch then?" →
`schedule_switch{ref}`. "Something light under 2 hours" → `categories{kind:"movie"}` →
`browse{kind:"movie", group:"comedy", max_minutes:120, unwatched:true, sort:"random"}` →
`details` → `play`. "What's on BBC One at 9" → `whats_on{channel:"BBC One", at:"21:00"}`.

**Casting (§40, cast design §12).** `cast_devices` (read-only, never scans) returns `{devices: [{ref:
"device:<32 hex>", name, kind, paired, available, casting}]}` with no addresses.
`play` takes `device` (a ref or an exact name): the reply is immediate (`casting_started`), the
agent confirms with `status`, whose `casting: {device, kind, phase, behind_live_s?, error?}` carries
the typed reason of the last failure (cleared when the TV shows a picture, or when the user ends
a cast that was not lost). Outcomes: `pin_required` (a locked item; the PIN is typed in Tuner),
`pairing_required` (the user pairs in Settings ▸ Cast; no tool takes a PIN), `unavailable` with
`cast-unreachable`, `cast-firewall`, `cast-unsupported` or `multiview-on` (`cast-firewall` only
from a fresh firewall detection made toward that TV's own address: ufw rules are scoped by source,
so one made toward another TV or the mDNS group never refuses; cast-design §9.4). `control` gains
`stop_casting` and `play_here`; `volume` answers `unsupported` on AirPlay; `popout` / `window`
answer `unavailable: casting`. `diagnose` gains a `cast` section (model, tvOS version, pairing
variant, timing, firewall state, last error kind). Placement: a `play` with `device` skips §30.13's
window placement entirely (`opened_in: "device"`); a popped-out window is put back in, unraised.
The bridge never retries a `device` play without the field on an older Tuner (it answers "The
running Tuner can't cast; update Tuner"). Casting calls are actions (the 2/s bucket). No tool
changes the firewall. There are 30 tools with `cast_devices` and `watchlist` (§42). Whether a cast needs a question is the
agent's protocol (the MCP instructions, `RULES.md` section 12, the tool descriptions), not the
server's: naming the TV is the user's yes, and so is "on the TV" when exactly one TV is `paired`;
with two or more paired TVs the agent asks which; unpaired TVs (only discovered) don't count;
with none paired it is the old behaviour (`full_window` when no TV is listed, `pairing_required`
when one is). `play {device}` always names its TV; nothing resolves "the TV" server-side.

### 30.6 Guardrails
* **Visibility = the screens' rules.** Results only contain what the snapshot shows: hidden
  channels / groups, the category filter (§28), explicit content while hidden (§20.3), stacks
  (one row per title, §29.11), `in_all` for provider lists (`categories`, `browse`). The handler
  additionally conceals the snapshot's explicit items when `ParentalView::hides_explicit` (a
  snapshot built while unlocked, relocked since) and checks DB rows with `Library::conceals`
  semantics under the live view. Concealed things are `not_found`, never "locked", never counted
  (`total`, `count`, `truncated`, `other_copies` are computed after filtering), never named in
  errors.
* **Locked ≠ hidden.** Locked items are returned with `locked: true` (like the dimmed rows);
  their details are withheld while the session is locked; `play` sends the play request and the
  app shows its PIN prompt (`outcome: pin_required`). No tool takes a PIN; the descriptions and
  the instructions tell the model never to ask for it.
* **No URLs, logins or internals.** Result structs have no URL fields (no logos, posters,
  stream or guide URLs, no provider ids, no `programme.id`); every reply is checked on the wire
  (`"://"` → replaced by `internal`, logged at WARN) and passed through `redact`. Player errors
  are reported as the kind (`http-forbidden`), never the provider's message.
* **Peers and limits.** Same-uid only (existing). At most 4 agent requests in flight (`busy`),
  queries ≤ 20/s (burst 40), actions ≤ 2/s (burst 5) (`rate_limited`); limits clamped; text ≤ 200
  chars; reply ≤ 1 MiB.
* **Logging.** Actions at INFO by method and ref only (`agent: play channel:2:xt:live:1 →
  pin_required`), queries at DEBUG without their query text at INFO+; never a title.

### 30.7 Launch, loading, old instances
* No instance: `ExeLauncher` → wait for the socket (≤ 15 s; a background `--refresh` holding the
  lock shows up as "didn't start" with its reason) → `loading` until the first library build
  (≤ 20 s). A started-by-MCP Tuner opens its normal start screen on the current workspace —
  except for a play asked for in the pop-out (`open_in: popout`, §30.13): `tuner
  --agent-popout`, the pop-out only (pinned, without the keyboard) on the current workspace.
* An instance from before v2 → `TooOld` ("restart Tuner to update it").

### 30.8 Auto-switch reminders (`platform::notify`, `store::user`, `app.rs`)
* Row: a reminder with `lead_s = Reminder::SWITCH_LEAD`; `add_reminder` as today (unique per
  channel + start); `schedule_switch` on a programme that already has a notify reminder turns it
  into a switch (`store::user::set_reminder_switch`).
* Scheduler: switch reminders run whatever `reminders.enabled` says (the scheduler starts
  always; disabled notification reminders are skipped, not switch ones). At `start −
  SWITCH_WARN_S` → `PlatformEvent::SwitchSoon` (the app toasts "Switching to ‹channel› in 10 s ·
  ‹programme›" with `("Cancel", Action::CancelSwitch(id))`, 10 s); at `start` →
  `hypr::focus_self()` + `PlatformEvent::SwitchNow` → the app plays the channel
  (`play_channel(.., PlayOrigin::Reminder)`: PIN rules apply) unless it is already playing it;
  the row becomes `Fired`. Cancel → row removed, scheduler refreshed. Tuner closed at the start:
  on the next start a switch up to 15 min late still happens, older ones are dismissed
  silently. Explicit channels' switches wait while hidden, like their notifications.
* Settings ▸ Reminders lists switches with their own glyph ("Switch") and Delete cancels them.

### 30.9 Skeleton API (branch `mcp/base`)
Implemented: `agent::proto` (all wire types, params, results, `AgentRef`, `parse_when`,
`fmt_time`), `agent::shared` (`AgentShared`, `ParentalView`), `agent::tools` (13 tools,
`INSTRUCTIONS`, `PROTOCOL_VERSIONS`), `agent::{AgentCommand, AgentPlay, PlayTarget}`,
`ipc::IpcServer::start_with_agent` + v2 dispatch, `cli::Command::Mcp` + `main` dispatch,
`ParentalSession::{with_unlocked_at, unlocked_at}`, `Reminder::{SWITCH_LEAD, SWITCH_WARN_S,
switches}`, `PlatformEvent::{SwitchSoon, SwitchNow}`, `Action::CancelSwitch`,
`AppEvent::Agent`, `Services::agent` + the handler wired into the IPC server.
Safe defaults: `AgentHandler::handle` → `unsupported` for every method; the app ignores
`AppEvent::Agent` and `Switch*`. `todo!("MCP-bridge")`: `client::{ExeLauncher::launch,
Client::call}`, `mcp::{main, serve}` (only `tuner mcp` reaches them).

**App side (implemented, branch `mcp/app`).** Modules: `agent::query` (the read-only tools over
a `Q` = effective snapshot + reader + `ParentalView` + clock + config), `agent::act` (what
`play` / `schedule_switch` / `cancel_switch` decide, `status`), `agent::guard` (in-flight and
token-bucket limits, the reply check), `agent::server` (dispatch, reader pool, budget, the
lazy-details wait, INFO logs of actions by ref). Notes and deviations from the text above:
* The app publishes the snapshot and the unlock instant every frame (`TunerApp::
  publish_for_agent`, after each `sync_explicit`); a replaced snapshot goes to the memory
  maintenance thread. The handler conceals explicit content of a snapshot built while unlocked
  with `Library::hiding_explicit` (cached per `generation`) when the session relocked since.
* `whats_on` covers the All-playlists scope (`in_all`) unless a channel is named; `guide_search`
  merges a programme's rows by (title, start as shown) across the channels showing it (≤ 10
  `also_on`); `locked` in results means "needs the PIN now" (locked and the session locked).
* Provider text (titles, descriptions, plots, cast…) has its links replaced by `[link]`
  (`query::clean`): EPG descriptions often carry a web address, which the wire check would
  turn into an `internal` error for the whole answer. The wire check stays the backstop.
* `play` of an Xtream series whose episodes aren't loaded submits `LoadSeriesDetails`, waits
  ≤ 3 s and decides again (`unavailable` if still loading). The app plays agent targets with
  `PlayOrigin::Cli` (channels, catch-up) / `Fullscreen` (titles, as Enter on a card); a locked
  title or catch-up waits in `TunerApp::pending_agent` behind `Pin { then: OpenPlayer }`.
* The reminder scheduler always runs (`ReminderScheduler::start(.., notify, ..)`): notification
  reminders are skipped while `reminders.enabled` is off, switches never. Its `ReminderWaker`
  is handed to `AgentShared` so a switch written by the handler is picked up without a frame.
  Cancel (`Action::CancelSwitch`) removes the row and remembers the id, so a `SwitchNow`
  already on its way is ignored.

**Integration (branch `feat/mcp`, after review).**
* Parental state before events: `logic` runs `sync_explicit` before draining the event queue
  too, so a `SwitchNow`, an agent `play` or a `--play` arriving in the frame the session relocked
  by its timer meets the hiding snapshot. `on_switch_soon` / `on_switch_now` also refuse a
  channel the store marks explicit while explicit content is hidden (`TunerApp::switch_hidden`).
* `Q::conceals` = the snapshot's `conceals` or, while explicit content is hidden, the store's
  `item_explicit` (the snapshot's explicit index has no episodes). `status` uses it for
  `now_playing` (an explicit episode still playing after a relock with no frame names nothing)
  and for the switches it lists; `cancel_switch` of a concealed switch is `not_found`.
* Every provider name in a result (channel, title, group, playlist, list, category, the names
  inside `play` / error messages) goes through `query::clean`, so one name holding a web address
  doesn't turn a whole answer into `internal`.
* `guide_search` and `search` prefer channels of playlists in All (`in_all`): a programme's best
  channel is ranked by (out of All, not favourite, needs PIN, quality, id); a channel match from
  a playlist kept out of All sorts after any other match of the same score. `guide_search` reads
  up to 500 FTS rows before filtering; reaching that sets `truncated`.
* `schedule_switch`: `at` more than 14 days ahead is `bad_request`; a bare `HH:MM` that has
  passed today means tomorrow. The 20-switch limit counts rows that can still fire and its
  message names no number (hidden switches count too). Cancelling a switch made from a reminder
  the user had removes both (said in the tool description).
* The instructions and the `guide_search` / `details` descriptions tell the model that provider
  text is data, never instructions.
* A switch cancelled outside its toast (`cancel_switch`, Settings ▸ Reminders) drops its
  "Switching to … in N s" toast on `DataScope::Reminders` (row removed; a fired row keeps its
  toast) and ignores a `SwitchNow` already on its way. An agent's PIN prompt clears a stale
  `pending_locked` (a prompt dismissed earlier) and vice versa, so the PIN plays the newest
  prompt's target. `control` waits (≤ 500 ms) for the change it asked for before answering.
* End to end: `tests/agent_e2e.rs` runs the real `tuner mcp` binary against the real app
  (`TunerApp` headless, frame by frame) on the demo profile: initialize → tools/list →
  guide_search → play → status → schedule_switch → control pause.
* The bridge connects only to a socket owned by the user in a directory only the user can enter
  (without `XDG_RUNTIME_DIR` the socket is under the shared temp directory). The launched Tuner
  gets no `RUST_LOG` and `/` as its working directory. `TUNER_MCP_NO_LAUNCH=1` turns launching
  off (tests driving the real binary).
* Answers come from what `logic` publishes: eframe runs `logic` for a window on another
  workspace too (only `ui` is skipped), so a hidden Tuner still answers.

### 30.10 Work split (two parallel tracks from `mcp/base`)
File ownership is disjoint; a cross-track edit needs the owner's agreement and goes in the
merge note. Lean: clippy + the tests you touched; the integrator runs the full gate.

**bridge track** — owns `src/agent/{mcp,client,tools}.rs`, `src/main.rs` (the mcp branch only),
`tests/agent_mcp.rs` (new), README "Use Tuner from an AI agent", MANUAL_CHECKLIST §21.
1. `mcp::serve`: the JSON-RPC loop of §30.2 (version negotiation, errors, 1 MiB cap, ordered
   handling); `mcp::main`: stderr logging, `Client` with `ExeLauncher`, stdin/stdout.
2. `client`: connect / send / read (caps, timeouts), launch-if-needed, loading retries,
   old-instance detection, `status` without Tuner → `{"running": false}`.
3. `tools`: tune descriptions (keep them < 1 200 chars, keep names and argument shapes).
4. Tests (`tests/agent_mcp.rs`, real protocol): `serve` over in-memory pipes — initialize
   (known / unknown / old version), `tools/list` (13 tools, schemas), `tools/call` round trip
   against a **real `IpcServer::start_with_agent` with a fake `AgentService`** on a temp socket
   (ok → `structuredContent` + text; error → `isError`), bad arguments → `isError` without an IPC
   call, unknown method −32601, garbage −32700, notifications produce no output; launch-if-not-
   running with a fake `Launcher` that binds the server after 300 ms (call succeeds; `status`
   and `control` don't launch); `loading` retried until ready; a v1-only server → `TooOld`; and
   one test of the real binary (`CARGO_BIN_EXE_tuner mcp --profile-dir <tmp>`: initialize +
   `tools/list` + `status` → `running: false`, nothing but JSON on stdout).

**app track** — owns `src/agent/{server,shared}.rs` + any new `src/agent/*.rs` it adds
(`query.rs`, `guard.rs`…), `src/ipc.rs`, `src/app.rs`, `src/services.rs`,
`src/platform/notify.rs`, `src/store/user.rs` (switch helpers), `src/ui/screens/settings.rs`
(Reminders glyph), `tests/agent_app.rs` (new).
1. Handler: dispatch, readers, budget, concurrency / rate limits, reply checks (no `"://"`,
   redact), INFO logging of actions by ref.
2. Queries: every tool of §30.5 over the snapshot + readers, with the guards of §30.6.
3. App: publish the snapshot and the unlock instant; `on_agent` play paths; `Switch*` events,
   Cancel toast, `Action::CancelSwitch`.
4. Switch reminders: scheduler changes, `set_reminder_switch`, Settings glyph.
5. Tests (real behaviour, seeded store / app `Rig`): each tool against a seeded library
   (channels in two playlists incl. `in_all` off, a hidden group, a category-filtered group, a
   locked channel, an explicit group, movies stacked across playlists, a series with progress,
   EPG with past / live / upcoming programmes incl. a tvg-shift channel); explicit content
   absent from every tool and every count while locked, present once unlocked, absent again
   after the relock timer **without a UI frame**; `play` of a locked channel → `pin_required` and
   the app opens the PIN overlay (no player command until the PIN); play of past programme with
   / without catch-up; `schedule_switch` → `SwitchSoon` toast with Cancel → `SwitchNow` plays the
   channel (and Cancel prevents it); no `"://"` in any tool's output on a library whose every
   row carries URLs; rate limit and `busy`.

**Integration**: merge app, then bridge; extend `tests/agent_mcp.rs` with one end-to-end run of
the real binary against a headless `Services` on a seeded temp profile (search → play →
`status`); full gate.

### 30.11 Risks
* **The model acts on a wrong match** ("touring car" → a documentary): the tool returns
  `airing` and channel, the instructions ask to confirm upcoming switches; `play` is undoable
  with Backspace (last channel).
* **Hidden content leaking through a side channel** (counts, `also_on`, errors): every count is
  computed after filtering; the tests check counts and errors with explicit content present.
* **Stale snapshot while hidden**: the handler's parental check doesn't depend on the UI; a
  library rebuilt while the window was hidden arrives when the UI runs (focus on `play`).
* **Spec drift**: the MCP subset is small and version-negotiated; unknown versions get our
  newest and the client decides.
* **Launch surprises**: only tools that lead to watching launch Tuner; `status` never does.

### 30.12 Naming the agent (user decision, 2026-10-12)
`tuner mcp` works with any MCP client, so Tuner's own screens don't say "Claude": they name the
agent that made a change by the name its client reports, store it with the change, and fall
back to "your agent".
* **Source.** `initialize.params.clientInfo` (`name`, optional `title`), forwarded on each IPC
  request (§30.3). `model::AgentName::from_client` maps it to a friendly name with an ordered
  table matched on whole words of the name or title (`claude-code` → Claude, `codex-mcp-client`
  → Codex, `gemini-cli-mcp-client` → Gemini, `cursor-vscode` → Cursor, …; the table in
  `model::agent` is the reference). No row matches → the title, then the name, unless it is a
  generic SDK default (`mcp`, `test`, …) or reads as the user or the app (`you`, `user`,
  `Tuner`, …: "You added Arsenal" only ever means the user); otherwise unknown. A malformed
  `agent` on the wire reads as absent, never a refused call.
* **Shown:** "Codex added Arsenal · 3 reminders", "by Codex" / "added by Codex" (Following, the
  follow sheet, spoiler rules), "marked seen by Codex", "Accept Codex's 55 fixes?", "Codex fixed
  the guide for 55 channels", "55 changes · Codex" in Recent changes. Unknown: "your agent",
  "Your agent" at a sentence start, "your agent's"; a real name keeps its own case ("iFlow").
* **Client-supplied text, display only.** Sanitised wherever it enters (`AgentName::named`):
  letters, digits, spaces and `-_.+'()` only, so no control or bidi characters, markup, `://`,
  newlines or blank letters (Hangul fillers); trimmed; at most 32 chars; empty = unknown. Never a permission: any local client can
  claim any name, so the caps, `spoiler_guard remove` and `guide_fix undo` check agent vs user
  (`origin 'agent'`, `FixActor::Agent`), never the name. Never in desktop notifications;
  `initialize` is logged at DEBUG only.
* **Stored** (migration `0013_agent_names`): a nullable `agent` column on `follow_rule`,
  `spoiler_rule`, `spoiler_seen` and `guide_fix_log`, written only for agent-made rows and kept
  through edits. Rows an agent made before 0013 are backfilled to 'Claude' (only the Claude Code
  plugin could make them), so NULL afterwards only means unknown. Stored keys are unchanged
  (`origin 'agent'`, `actor 'claude'`; `FixActor::Agent` still serialises as "claude").
  `channel_epg_map.actor` and `queue_item.added_by` get no name: neither is shown.
* **Backups** (format version stays 1): the name is always written (`null` = unknown); a backup
  without the key predates names and reads as Claude, like the backfill; older builds ignore it.
* **Upgrades.** A new bridge talking to an older running app: the app ignores `agent` and keeps
  saying Claude. An older bridge talking to a new app: no name, so "your agent".
* The Claude Code plugin (§37) and `tuner claude-setup` stay Claude Code specific; the tool
  result field `by_claude` became `by_agent` (RULES.md follows).

---

### 30.13 Where an agent's playback opens (decision record, 2026-10-12)
User report: "I tested out the play from claude and it started the mini player in the app.
What I meant for this feature would be to start it detached as the multi workspace visible
player" — the pop-out (§21.6). Decision: what an agent plays opens in the pop-out by default
(amended the same day: unless Tuner's window is on screen, step 6 and its follow-up),
the way scheduled playback does (§30.8, reminders "Open in"), and the agent says where.
* **Where.** `play` and `queue` (`play`, `skip`) take `open_in`: the reminders' names,
  `popout` / `full_window` / `where_i_am`. Settings ▸ AI agents "Agents play in" (`[agents]
  play_in`, `pop-out` (default) / `full-window`; an older config loads as the pop-out) is the
  default (a hand-written `where-i-am` counts as the pop-out, which the screen then shows; the
  screen says *Pop-out* / *Tuner window*). `act::agent_show(asked, setting, window, pin)`,
  pure, in this order:
  1. `full_window` → the window, raised, on its player, **fullscreen** (the tool says: when the
     user asks for full screen or the big screen, or the TV when `cast_devices` lists none;
     `AgentShow::fullscreen`: set at once,
     or once a pop-out is back in, `fullscreen_after_pop_in`);
  2. the item needs the PIN → the window, raised (the PIN is typed there);
  3. `where_i_am` → only what plays changes (the screen stays; the mini player shows it);
  4. no Hyprland (no pop-out) → the window, raised, as before;
  5. `popout` → the pop-out, also out of a fullscreen window (asked for);
  6. the setting: Tuner window → the window, raised. Pop-out: a pop-out stays the pop-out (one
     still pending from a launch counts); two pictures on (§35) stay in the window, raised when
     it is out of sight; Tuner's window **on screen** → its full window, **not raised** (it is
     where the user looks; the terminal keeps the keyboard), whatever screen it shows — the
     player is pushed over Live TV's preview, Home, Movies… — and a fullscreen window stays
     fullscreen (the window is not made fullscreen otherwise); the window **out of sight**
     (on a workspace no monitor shows, fullscreen or not) → the pop-out.
  Follow-up (user report, 2026-10-12: "When I minimised after that to in app preview every
  launch after that just stayed in preview. It was intended to have it launch in the popout
  player IF the running app is not in the same workspace as the agent terminal … If its in the
  same workspace it would be full window width (not necessarily fill screen)"). That report
  came from an 0.2.4 build from before this section's first change (no `--agent-popout`, no
  `opened_in`): there `queue play` only fed the corner mini player (the Live TV preview), as
  "Up next" below describes. The rule it states still stands and changes step 6: it used to
  keep the window only when the player or a fullscreen window was on screen, so a window on
  screen showing Live TV, Home or a film page popped out; now being on screen alone decides.
  A window under another one fullscreen (or maximized) on its workspace — the terminal after
  Super+F — is out of sight (`j/clients`): the play pops out.
  "On screen" (`WindowNow::visible`) is not keyboard focus: an agent's user types in a terminal,
  so Tuner is almost never focused when a play arrives (tiled beside it, on a second monitor).
  The app publishes `visible = focused`; when the answer depends on it (unasked, setting
  Pop-out, not popped out, not focused), the handler asks Hyprland at play time (`platform::popout::self_shown`, `spawn_blocking`: `j/clients` +
  `j/monitors` — our window pinned, or on a monitor's active or open special workspace, not
  a hidden group tab, and not under another window fullscreen on its workspace). When Hyprland can't tell (also a window not mapped yet: Tuner just
  started for this play, below), the window counts as seen (nothing moves uninvited).
* **Focus.** The handler calls `hypr::focus_self()` only when the window is raised (before, it
  raised on every play: the main window jumped in front). The pop-out never takes the
  keyboard (§21.6).
* **Saying where.** The result has `opened_in: popout | window` (`OpenedIn`); `play`'s message
  ends "It's in the pop-out." / "It's in Tuner's window." (+ "the pop-out needs Hyprland" when
  `popout` was asked off Hyprland). The skills tell the user.
* **The window's facts.** The app publishes `reminders::ui::WindowNow { focused, watching,
  popped_out, can_pop, fullscreen, multiview, visible }` to `AppShared::window` every frame (`logic`
  runs while the window is hidden too). Before the first frame `can_pop` is whether Hyprland
  runs (the worker starts with the first frame; a pop-out asked for then waits for it through
  `reminder_ui.pop_out_pending`, like a switch reminder's launch).
* **Up next.** `queue play` and `skip` used to only tell the engine, which sends the play
  straight to the main picture: the app never heard of it, so the window stayed on Home and the
  corner mini player showed the film (the user's report was such a `queue play`). Now the
  handler sends `AgentCommand::Show(AgentShow)` after the engine answers `Playing`: the app
  pushes the player and places the window like a `play`. A `WaitingForPin` shows the window,
  raised. A `skip` while Up next already drives the picture leaves it where it plays (like a
  zap; `opened_in` says where that is); the next items play into the same picture, so a
  pop-out stays a pop-out. A later locked item waiting for the PIN in the pop-out sends the
  "Up next is waiting for your PIN" notification (as for a hidden window: the pop-out never
  takes the keyboard). Movies and episodes resume as before: only the window moves.
* **"Put it on the big screen".** `control window`: `AgentCommand::Show` with the window,
  raised and fullscreen, for what already plays (nothing restarts; `unavailable` when nothing
  plays).
* **The PIN and a launch.** A launch for the pop-out asks for it before the worker runs
  (`reminder_ui.pop_out_pending`). A play that then goes to the window (the PIN) drops that
  request (`agent_place`), so the prompt never lands in the pop-out, which can't take the
  keyboard. `agent_play` reports a channel's prompt only when this play opened it (a
  `pending_locked` left by a prompt dismissed earlier says nothing).
* **A play that fails** (`play` or `queue play` / `skip` erring, or Up next with nothing to
  play) while the published window is popped out (or about to be) and the main picture idle
  sends `AgentCommand::PopIn`: the empty pop-out goes back into the window, unraised, instead of
  staying pinned on every workspace saying "Nothing playing".
* **Tuner not running.** The bridge decides with the same function, the window counted on
  screen (Hyprland opens a new window on the current workspace: the one the agent's terminal
  is on). So an unasked play starts Tuner's window as a plain `tuner` would (tiled, normal app
  id, focused like any app launch), and the play fills it (step 6: before the first frame nothing is published, the window
  isn't mapped or is on screen, so the handler decides the window too; no pop-out is pending).
  Only `open_in: popout` under Hyprland starts `tuner --agent-popout` (`client::Launch::PopOut`)
  — the switch reminders' launch (`tuner-popout` app id, the packaged no-focus rule, popped out
  on the first frame), showing "Nothing playing" for the second until the play arrives.
* **Multi-view** stays in the main window: an agent's `multiview open` while popped out pops
  back in and raises the window first (§35.3).
* **Unchanged.** `control previous_channel`, zaps and `control popout` act wherever playback
  is.
* **Older Tuner.** `Play` and `Queue` refuse unknown fields; a Tuner from before `open_in`
  still running after an upgrade answers `bad_request` naming it. The bridge then asks again
  without `open_in` (that Tuner's own play: its window) and adds a `note` to restart Tuner.
* **Known race.** `tuner --agent-popout` that loses the start race to a Tuner starting at the
  same moment hands off `popout: true` with no play: that Tuner pops out empty until the
  agent's play arrives (it goes there), or the failed play puts it back in (above).

## Batch v0.2: five features, reminders, Claude skills (§31–§38)

The v0.2 batch adds five features designed together (big-features plan, 2026-10-11), the
second reminder pass with the sleep timer, the Claude Code plugin, playlist codes and a faster
library load. Each feature has a kill switch (`ENABLED` in its module: `guide_fix`, `queue`,
`follow`, `spoiler`/`guard`, `ui::multiview`, `reminders`); all are on. The approved designs are
kept in `docs/design/*-design.md` (user stories, mockups, numbered rules), and each build
track's notes in `docs/design/<track>.md`. The sections below are the contract as built.

**User decisions (2026-10-11).**
1. Order: Guide fixes → Up next → Following (with the news flash) → Spoiler guard → Multi-view.
2. Automatic things (Following rules, the news flash) never interrupt a film or episode being
   watched: they become a notification with **Watch**. The user's own one-off auto-switch
   reminders still interrupt, with the 10 s Cancel. With multi-view on, automatic switches go to
   the silent picture.
3. The spoiler guard is on, but narrow: part-way series and followed sport, with a quick reveal.
4. Multi-view: the sound stays where it is until `M`; moving a picture to its other playlist
   always asks. Both of the user's accounts allow one connection each, so the two pictures come
   from different playlists.
5. An agent never unlocks, reveals explicit content or loosens a setting.
6. Every design's open questions took the recommended default.

**Shared blocks** (plan §1): B1 one play router (`playback::route`: who may take which picture,
and what an automatic play becomes); B2 `AppShared` (the published library, pictures, Up next,
the guard, the parental view) for code off the UI thread; B3/B4 the spoiler guard as one API
(`guard::Guard`) that every outside text goes through; B5 copies (`store::copies`: a channel's
or title's copies across playlists, ranked); B6 reminder hooks (rule-owned rows, switch modes,
systemd arming); B7 `jobs::after_guide` (after a guide import or rematch: Following, then the
guide check); B8 the effective time shift (`COALESCE(item_override.shift_min,
channel_epg.shift_min, channel.tvg_shift_min)`, one select for the library, the OSD and
catch-up).

**Migrations** 0008 (reminder modes), 0009 (guide fixes), 0010 (Up next), 0011 (Following),
0012 (spoiler guard), 0013 (agent names, §30.12). All are additive; older builds ignore the
new columns and tables.

## 31. Guide fixes

Design: `docs/design/guide-fixes-design.md`. Build notes: `docs/design/gcore.md` (engine) and
`docs/design/gui.md` (screen).

### 31.1 What it is
The EPG mapping screen becomes **Guide fixes** (Ctrl+E, "Assign guide channel…", Settings ▸ EPG
"Fix guide data… (N to fix)"). It lists what is wrong with the guide data, one row per channel,
proposes a fix, and applies fixes in logged batches that can be undone.

Issue kinds, in the order a channel is classified (each channel gets at most one; a dismissed
kind is skipped): Gone from guide, +1 without shift, Looks wrong, Guide ended, Copy has guide,
Suggested, No guide. Each row's "why" is a fixed phrase; no URL ever appears.

### 31.2 Rules derived after every rematch
* **+1/+2 follow their base.** `textmatch::timeshift::marker` parses a strict `+N` / "plus N"
  (1–24 h) at the end of the name, after quality words; "Canal+ 1" and "Disney+ 1" are not
  markers, "E4+1" and "Quest Red + 1" are. A "+N" channel without its own guide takes its base's
  guide channel with `shift_min = N·60`.
* **Copies share a guide.** A channel with no guide takes the guide channel of its first
  resolving copy (B5 family key, another playlist, usable, no tvg-id veto).
* Rules never touch manual rows. Resolution order: manual, sibling, guide priority, method.
* Both rules are per playlist (`SourceSettings::guide`), toggled on the Rules card (always with a
  confirm naming how many channels change).

### 31.3 Batches, Undo, the shift
* `store::guide_fix::apply` writes one batch (`guide_fix_log` + one change row per field: `map`,
  `shift`, `catchup`, `dismiss`, `rule`). Undo restores each field only while it still holds the
  batch's value; a field changed since is skipped and named. A restored manual mapping keeps who
  chose it (older logs fall back to "you").
* A time shift moves the channel's reminders, Up next slots and Following rows in the same
  transaction (`store::retime::channel_shift`).
* The catch-up offset: the channel's own (`Option`, so a stored 0 wins), else the playlist's,
  else 0. A user time shift never changes the provider's `tvg_shift_min` sent to catch-up.
* Shift and offset presses are debounced (400 ms, or on leaving the row/screen); the screen
  bases a quick second press on what its last flush wrote.

### 31.4 The guide check (`jobs::guide_check`)
* Fuzzy suggestions for unmapped shown channels are cached per **stamp** (guides' and playlists'
  generations, the rules, the threshold). Channels without a candidate get a marker row, so each
  channel is scored once per stamp. The cache floor is the lower of 0.8 and the user's threshold.
* **Debounced before it is submitted** (30 s; a request in the window pushes the deadline back),
  so it never holds the "features" lane or shows as a job while it waits. Requested after a
  guide import or rematch, a playlist import, an applied or undone batch that changed rules or
  mappings (the screen's and an agent's), and a threshold change. The GUI checks the stamp once
  after its first library publish. A headless `tuner --refresh` never checks (`JobContext::
  headless`).
* "Include hidden" channels aren't scored by the check (tens of thousands); the screen scores
  the rows it draws.

### 31.5 Guardrails
* The universe is the library snapshot: explicit, hidden and filtered channels follow its
  visibility (counts and coverage too).
* An agent's batches: at most 200 channels; more than 50 isn't applied from chat
  (`pending_review`): the handler hands the proposal to Tuner (`AgentCommand::GuideFixReview`,
  the window raised, out of the pop-out), which opens Guide fixes with its confirm card
  prefilled ("Accept Codex's N fixes?", named as in §30.12). Enter applies it as the agent's
  batch (toast with Undo; `stale` if the guide or a playlist changed meanwhile: nothing applies
  and the toast says so); Esc drops it, and the agent proposes again or a narrower batch. A newer
  batch replaces one still waiting for its card (toast "Codex's newer guide fixes replaced the
  earlier ones"); a card already open is never replaced (the newer one waits for it). A batch
  applied from chat toasts "Codex fixed the guide for N channels" with Undo;
  proposals live 10 minutes and are bound to the stamp; locked channels and rules for a
  playlist with a PIN-locked channel are refused while locked; an agent undoes only batches an
  agent made (whichever agent), never the user's.
* Backup carries time shifts, dismissals and manual mappings with their guide name and who chose
  them (restore keeps a shift already set on this machine).

## 32. Up next

Design: `docs/design/up-next-design.md`. Build notes: `docs/design/qcore.md`,
`docs/design/qui.md`.

### 32.1 What it is
A play queue of films, episodes and live programme slots (`queue_item`, migration 0010).
Ctrl+N adds, Ctrl+Shift+N plays next; Alt+6 opens the Up next screen; `u` in the player opens
the panel, `n` / `N` step. Home shows an "Up next" rail while items wait and nothing plays.

### 32.2 The engine (`queue::service`, `queue::machine`)
* Plays the first row that is pending, visible (not concealed), available and not deferred.
  Played, skipped and failed rows stay in place, so "Next" is exactly the play order.
* Drives the main picture only while `origin == Queue` and the playable is one of the row's refs
  (any copy of the title, any ranked channel of a slot). Any other pick or zap steps aside
  ("Up next paused · Alt+6 to carry on"); the row stays current. An interrupted row is never
  marked skipped by `n`.
* Countdown between items (`[queue] countdown_s`: at once, 5–30 s, or Ask); a live slot gets a
  30 s card at its end; a future slot waits; after a suspend a slot plays from its archive or
  is skipped as "Expired".
* Every play goes through B1 (`PlaySource::Queue`); in multi-view Up next waits.
* An agent's `queue play` (and `skip` when Up next wasn't playing) shows where `play` would —
  Tuner's window when it's on screen, else the pop-out, `open_in` — and the following items
  stay there; a `skip` inside a
  running Up next keeps it where it plays (§30.13).
* With Up next empty, an episode ending outside it offers the next episode with a countdown
  (the engine, in any placement, a hidden window included). Before it plays it re-reads the
  lock and concealment: a series locked again says "Next episode is locked" and plays nothing.
  When the queue runs out after an episode, the next one is offered with no countdown.
* A queued film ending in the fullscreen player keeps the player (the next item follows on the
  card); only films started elsewhere return to their page.
* The player's OSD row has `Up next` (the panel, `u`) and, while Up next has a next item and
  multi-view is off, `Next` (`n`), before Info; narrow keeps `Next` only. Both are in the
  controls focus walk, and a button that comes or goes keeps the focus on its control.
* Enter (a click, "Play now") on a film or episode row with progress past
  `resume_min_s` opens the usual Resume prompt (the latest progress of any copy, as the
  resolver resumes it); its answer goes to the engine as `PlayAtFrom { id, resume }`, which
  overrides the row's "from the start" for that row only. A row that asks for the start, a
  programme slot, and locked, concealed or gone rows play at once as before (the engine says
  why). Advancing never asks.

### 32.3 Guardrails
* Locked rows wait for the PIN (`Blocked::NeedsPin`; the engine never unlocks; a hidden window
  gets "Up next is waiting for your PIN" through `Guard::outside_title`). Concealed rows are
  left out of rows, counts, ETA and cards, never played and never deleted: Clear removes the
  visible rows only (also over MCP).
* Next-episode labels go through the spoiler guard (`Subject::Episode`); the S/E number always
  shows.
* Three failures in a row pause Up next with a toast. Nothing plays after a restart.
* Media keys (MPRIS Next/Previous) go to Up next only while it drives the picture.
* Only rows still to play are listed under Settings ▸ Orphans.

### 32.4 Kill switch
With `queue::ENABLED` off, the player's own next-episode card is the fallback
(`tests/guide.rs::episode_end_offers_the_next_episode` guards it).

## 33. Following and the news flash

Design: `docs/design/following-design.md` (and the news-flash addition). Build notes:
`docs/design/fcore.md`, `docs/design/fui.md`.

### 33.1 What it is
Rules (`follow_rule`, migration 0011) that watch the guide for a team, a show, an event, a
news bulletin or custom terms, and set reminders, switches or news flashes for what they find.
Templates: Team, Show, Event, News flash, Custom. Set up from a programme (`R`, the popover's
"Follow…" / "News flash this daily…", the item menus' "Follow this programme…"), Settings ▸
Following ("+ Follow…", the rules in priority order, Upcoming for 14 days), or an agent.

### 33.2 Evaluation (`jobs::follow`, `follow::plan`, `store::follow`)
* Triggers: a guide import or rematch (B7), a mapping change, a rule change, a new library
  (hidden, locked or explicit channels change what may be picked; not the first library at
  start) and a daily tick. All coalesce into one job on the "features" lane; a queued
  evaluation that must look again keeps its reason when a guide/daily request joins it. Inputs
  are fingerprinted (guides, rules, mappings) so an unchanged guide/daily run exits early.
* One Aho-Corasick over every rule's folded terms, with an ASCII prefilter (0.8 s for 760k
  programmes × 100 rules).
* **Events.** Hits of one rule starting within 10 min are one event only when they are the same
  programme: a window is split by event key; fixtures that share one side with the other
  spelled differently ("Man Utd v Arsenal" / "Manchester United v Arsenal") stay one event,
  show episodes group by their core title.
* **Channel.** Pool channels (visible, supported, mapped, never explicit in any parental state)
  ranked by B5; a "+1" copy is never the live match; three alternates.
* **Ledger** (`follow_match`): states scheduled, listed, adopted, skipped, gone, over-cap.
  Weekly caps and `max_pending` apply. A reminder at the same channel and start is adopted, not
  duplicated; when the owning rule drops it, it is handed to another rule that adopted it.
* **Editing a rule** (mode, lead, offset, spoiler-safe, flash settings, or the name of a
  spoiler-safe rule) removes its pending reminders, and the evaluation makes them again with the
  new settings. A rule's offset that lands past a short airing's end isn't used there. Editing
  one reminder's lead, mode or offset detaches it from its rule.
* **Following off** holds every rule reminder (the scheduler and the closed path skip them).

### 33.3 Switches and interrupts
A rule's switch goes through B1 (`PlaySource::Reminder { rule }`): over a film, episode or
catch-up it becomes a notification with Watch (`follow.interrupt_vod` can allow it; never for a
news flash). A locked target while locked becomes "… · needs your PIN". Clashes: your own or
an agent's switches win, then rule priority, then the earlier start; the loser notifies.

### 33.4 News flash
* A News rule in Flash mode: daily or weekdays, matched from the guide each day (it follows
  schedule changes). It pops up in the pop-out without taking focus (Cancel / Open full), whole
  programme or **headlines only** (first N min, default 5).
* Never in quiet hours (`[follow.news_flash] quiet_from_min..quiet_to_min`, typed "22:00–07:00"),
  never while another app is fullscreen (`hyprctl -j activewindow`, with a timeout; only asked
  when that setting is on), never over a film or episode, and nothing when the bulletin (or a
  copy) already plays on either picture. The check runs at the countdown and again at the switch
  (`reminders::flash_skip`), in a running Tuner and on the closed path.
* Sound: Normal, or Mute ("silent flash": single view mutes the main picture; multi-view plays it
  in the silent picture without taking the sound). There is no "duck" (lower what plays under
  it): nothing plays under a flash with its sound (single view replaces what played, multi-view
  gives the flash's picture the sound), so it would act as Normal. A `"duck"` stored by an early
  build reads as Normal.
* At its end the previous playback comes back (the bulletin or one of its copies must still be
  playing; if the user moved on, nothing changes), the window pops back in and the Player screen
  is left if the user wasn't on it. In multi-view the flash's picture gets back what it showed,
  or closes if it showed nothing, and the sound returns where it was. A late flash on catch-up
  ends that much later; one whose headlines are already over doesn't start.

### 33.5 Feedback and agents
* "Following: Chelsea · 1 in the guide" with **Undo** for a new rule; an agent's rules toast
  "Codex added X · N reminders" (the agent's name, §30.12) with Undo (`Action::UndoFollow`).
* A summary notification at most every 12 h, only with additions, with **Open** (and a click on
  it): the window is raised and shows Settings ▸ Following (`FollowEvent::Open`). Without a
  notification server it is an in-app toast with the same Open (`Action::OpenFollowing`).
* Agents' rules are marked with the agent's name ("by Codex", "added by Codex") and capped
  (10 an hour, 20 in all, for all agents together); an agent can't turn a rule's spoiler-safe
  off; a concealed channel's match is named by the rule only.
* Upcoming rows in Settings go through the spoiler guard and skip concealed channels.

### 33.6 Settings ▸ Following
* Rule rows, in priority order (higher wins a clash): Enter or a click opens the rule in the
  sheet's editor. `x` / Delete / right-click / the gear open the row's menu: **Edit…**,
  **Pause / Resume** (the evaluation that follows cancels a paused rule's reminders, and makes
  them again on Resume), **Test against the guide** (a preview over 14 days in the background,
  toasted as "Arsenal: 4 in the next 14 days · next Sat 12:30 on Sky Sports Main Event · 2 left
  out (repeat)": the rule's name, times and channels only), **Move up / down**, **Delete…**
  ("Delete it and cancel its N reminders" / "keep the reminders as my own").
* Reorder: Alt+↑/↓, the drag grip (shown with two rules or more) or the menu's Move; the
  order is saved at once and the guide checked again. The Settings hint's "reorder" covers them.
* Upcoming rows: Enter, a click or Delete opens the programme's popover (Watch, Cancel reminder =
  skip this one, Follow…).

## 34. Spoiler guard

Design: `docs/design/spoiler-guard-design.md`. Build notes: `docs/design/score.md` (engine),
`docs/design/sui.md` (UI).

### 34.1 What it guards
On by default but narrow; nothing is guarded before an event starts. Display only: it never
builds events from explicit content, never lists or counts it, never reveals it.
* **Series you're part-way through** (automatic): episodes after the next-up one lose title,
  plot and still; guide airings with S/E after next-up likewise (the grid's slim rows carry the
  guide's S/E too); an airing without S/E is placed first (§34.5) and loses its sub-title and
  description when it comes after next-up or when nothing places it.
* **Sport you follow**: Following's spoiler-safe Team/Event rules become topics. An airing
  counts as the event only when it parses as a fixture (`parse_fixture`) and has none of the
  rule's "leave out" words ("Women", "U21", "Fan TV"…); highlights and reaction shows are never
  the event. Following topics don't hide every sport description.
* **Topics you add** (rule editor: teams from "A v B", aliases, scope, until watched / N h /
  always / a blackout from "I'll watch it later").
* A topic is active from the event's first airing until it is seen or `window_h` after it ends.
  Fixture keys group repeats on later days (one embargo, lifted by watching any airing within
  7 days).
* Scores in titles are rewritten ("Arsenal v Chelsea 󰈉"), including a trailing "2-1" or
  "(2-1)" after a fixture; a sub-title becomes a fixed-width bar; descriptions say why and how to
  show them.

### 34.2 Where it applies
Guide grid cells and tooltips, Live rows, details and upcoming list, search hits (painted text
and accessibility labels), Home's on-now line, the programme popover, series and movie pages,
Up next's labels, multi-view strips, notifications (`Guard::outside_title`, also when Tuner is
closed: built from the store, failing closed), MPRIS titles, Following's Upcoming and every
MCP reply (`guard::reply::guard_json`). Painters cache per programme, minute and
`Spoilers::stamp()`, so Ctrl+B and rule changes show at once.

### 34.3 Seen, reveal, shield
* **Seen**: live at ≥ 50 % watched or present in the last 10 minutes; catch-up/VOD at ≥ 90 %;
  either picture counts; Ctrl+W marks a guide programme seen (toast "Spoiler guard…" opens the
  menu with Undo).
* **Reveal**: Ctrl+B on an item (any move hides it again); Ctrl+Shift+B opens the menu; "show
  everything for 15 min / 1 h / until restart" with a status chip and ✕. The hint line says
  "Ctrl+B show spoilers" while the guard hides something of the focused item ("Ctrl+B hide
  spoilers" while it is shown). In the player, the banner's programme line ("Severance ·
  ▆▆▆▆▆▆▆▆ 󰈉") and next title go through the guard, the banner says "Ctrl+B show spoilers", and
  Ctrl+B shows the banner's now and next until the picture changes (a zap hides them again).
  The slim now / next rows carry no categories, so the banner reads each programme's categories
  once (frame budget, `player::reveal::BannerCats`) and shows its sub-title only once they are
  known: "sport descriptions" hides an unrelated match's sub-title there too.
* **Zap shield**: before tuning. A zap (↑/↓, digits, the channel panel) onto a channel whose
  programme mentions an active topic sends no play: the card covers the picture and the picture
  underneath pauses. Enter = Watch anyway (for as long as the same reason shields it), ↓ judges
  the next channel, Backspace/← resumes. A channel chosen in Live or the guide is never shielded.
  In multi-view the card covers only the picture that zapped (that picture pauses, the other
  plays on); its keys act while that picture has the focus, and a click on it focuses it first.
  One card at a time: a shielded zap in the other picture moves it there.
* The snapshot rebuilds on guard, library, guide, follow and progress changes, but not every
  progress save while a film, episode or catch-up plays.

### 34.4 Agents
`spoiler_guard` adds topics and blackouts (a programme blackout guards its two teams) and
marks seen; Settings shows such rules as "added by <agent>" and the marks as "marked seen by
<agent>" (§30.12). `remove` only removes rules an agent added, whichever agent ("that rule is
yours — remove it in Tuner ▸ Settings ▸ Spoiler guard"). Replies drop guarded fields unless the
call names the item in `reveal_spoilers`.

### 34.5 Series airings without S/E
Slim guide rows (grid, now/next, search hits) carry the guide's season and episode, so most
airings are judged by number. An airing of a guarded series without one is placed by
(`spoiler::verdict::airing_beyond`, in order):
1. its sub-title: an "S02E05" / "2x05" / "Season 2 Episode 5" marker, or a catalogue episode
   title of the series (folded; "Show - S02E05 - Title" names count as "Title"; a title two
   episodes share, or "Episode 6", places nothing);
2. a seen repeat: an airing watched (or Ctrl+W'd) before keeps its "title#sub-title" key as the
   series' seen state (`spoiler_seen.series_norm` set, no episode; never pruned), so its repeats
   show;
3. the catalogue's air dates: an airing a week or more before the first guarded episode first
   aired can't be one (the margin covers another country's dates);
4. the guide's "new" flag: a first showing is later than next-up, unless it airs before the
   next-up episode first aired.
What nothing places is guarded, also for a series the user guarded before starting it (their
own rule is never weakened); rules 2 and 3 still let seen and old repeats show.
Watching an airing a sub-title places records that episode, which moves the series on.
MCP replies have no airing date (start 0): rule 3 never applies there.

## 35. Multi-view

Design: `docs/design/multiview-design.md`. Build notes: `docs/design/mv1.md` (the second player),
`docs/design/mv2.md` (the UI), `docs/design/multiview-spike.md` (two UHD HEVC decodes on the M2).

### 35.1 What it is
Two live or catch-up pictures (A and B): side by side, big + small, or picture in picture
(`auto` picks by width; the pop-out and the mini player are always picture in picture). Ctrl+M
on a channel or programme ("Watch alongside") opens B; in the player it opens a channel picker
for the other picture. `w` / Ctrl+arrows / a click move the focus (keys go there), `M` moves the
sound, `t` cycles the layout, `W` swaps, `q` closes the focused picture (the other carries on
with the sound). Esc never closes a picture.

### 35.2 Rules
* **Connections.** One stream per playlist on both accounts, so the planner
  (`ui::multiview::plan`) never exceeds a known or learned limit: the pick, else a healthy copy
  on a free playlist, else (asked, never silent) "Move picture A to its TV 2 copy?" — only then
  `Tiles::rearrange`, which claims both pictures' new playlists before either loads. Locked
  copies are never picked while locked (`ZapTo` carries the session state), nor copies whose
  explicit flag differs.
* **Zapping a picture past the other's playlist** plays the next channel's copy elsewhere, or
  skips it with "BBC One skipped — TV is in use by picture A".
* **Films, episodes and files play in picture A only**; picking one ends multi-view.
* **Plays from outside** (a reminder's switch or Watch, `--play`, MCP `play`, a news flash, a
  late catch-up switch) go to the silent picture and take the sound; a silent news flash doesn't
  take it.
* **Sound and volume** act on the picture with the sound (volume, mute, sleep-timer and flash
  mutes included). If the picture given the sound fails while the other plays, the sound goes
  back.
* **Parental.** The PIN gate runs before a picture is chosen, for Ctrl+M and for an agent's
  two pictures alike; the stored plays go out once the PIN is typed and are forgotten on relock.
  Relocking stops only the pictures showing a locked channel or explicit content.
* Picture B starts lazily on the first request (its own mpv, muted, small caches) and lives
  until exit.

### 35.3 Agents
`multiview open` with two refs plans both pictures at once (one plan, so the second never
plans against what the first replaces): from single view the focused picture and the other,
with multi-view on A and B (the layout's places and the sound stay; both claims move before
either loads, so pictures that trade playlists aren't refused). A picture that already shows
one of the two keeps it, and the other goes in the other picture alongside it — moving the
picture that stays is asked first (the app's card, or `needs_rearrange` over MCP). One ref
goes alongside the focused
picture, judged against that picture's stream; `needs_rearrange` when only moving the playing
picture works; a film playing answers "multi-view is for live TV and catch-up". `control` with
no `tile` pauses/stops both, sets volume and mute on the picture with the sound, and acts on the
focused one for previous channel and pop-out. Opened while the window is popped out (an
agent's play went to the pop-out, §30.13), the window pops back in and comes forward first:
two pictures belong in the window.

### 35.4 Gaps
A PIN prompt cancelled with Esc keeps its stored plays until a relock (harmless: only `play_channel`'s or the agent's
prompts send them).

## 36. Reminders (pass 2) and the sleep timer

Build notes: `docs/design/remind.md` (engine), `docs/design/rui.md` (UI). Extends §30.8.

### 36.1 Reminders
* A reminder's **target** is `start + offset_s` (−1 h to +12 h; "10 min in" skips the
  build-up; an offset at or past the programme's end is refused for the user's own rows).
  Notifications go out `lead` before it; a switch counts down 10 s before it and switches at it.
  Migration 0008 stores the mode (`mode = 'switch'` with `lead_s = SWITCH_LEAD`).
* **The reminder sheet** ("Switch to this when it starts…" / "Edit reminder…" in a programme's
  menus): Notify me / Switch to it, lead, start offset, **Open in** (Pop-out — the default,
  pinned, every workspace, no focus taken — Full window, or Where I am), and up to 3 clashes.
* **Late**: a notification still goes out while its programme airs; a switch up to 15 min late
  still happens and always gets its 10 s countdown (one warned is not dismissed by the late
  limit); 60 s or more late it plays from the beginning on catch-up when the channel has it and
  needs no PIN.
* **The strip** (top centre, never takes focus): "Switching to ‹channel› in 8 s · ‹title›" with
  Cancel and Open full; Delete in the pop-out, and Esc in the player or pop-out, cancel the first
  countdown. A decision made at the countdown is made again at the switch.
* **Multi-view**: every automatic switch goes to the silent picture; a switch to a channel that
  already plays on either picture does nothing.
* Settings edits take effect live (the scheduler is refreshed, never restarted, so a countdown
  already shown isn't shown twice).

### 36.2 While Tuner is closed
Transient `systemd-run --user` timers (`tuner-reminder-<id>`, described only "Tuner reminder",
at most 30 within 48 h, a switch's 30 s before its countdown) run `tuner --reminder <id>` (the
running Tuner's own executable). Units take the user manager's environment (session variables
are set only when the manager lacks them), so a re-login doesn't leave stale values. A headless
`tuner --refresh` re-arms the units after Following's evaluation. With Tuner closed:
* a notification is shown from the short-lived process (Watch runs `tuner --play`), its title
  through the spoiler guard built from the store (failing closed);
* a switch starts Tuner (popped out unless the reminder says otherwise); a news flash in quiet
  hours or over a fullscreen app does nothing;
* the session counts as locked: explicit channels wait, a Following switch to a locked channel
  becomes "needs your PIN".

**Re-armed at login.** Transient units die with the user manager (reboot, or logout without
lingering), so a reminder set before a reboot used to be missed unless Tuner was opened first.
The package ships `tuner-rearm.service` (`/usr/lib/systemd/user/`, a oneshot running `tuner
--rearm`) with its own `graphical-session.target.wants/` link: enabled for every account
without a scriptlet touching home directories, removed with the package, `static` in
`systemctl --user is-enabled`, `systemctl --user mask tuner-rearm` turns it off. `tuner
--rearm` (`reminders::arming::rearm_at_login`) arms what a Tuner started then would arm
(`arming::closed_plan`: the scheduler's pending reminders, Following's waiting while it is off,
explicit channels' while parental control hides them — the session counts as locked) through
the same `ArmSync`, and exits: no network, no refresh, no window, player or notification.
* **When**: `After=graphical-session.target`, wanted by it. uwsm (Omarchy's session manager)
  reaches that target at every login, after `wayland-session-waitenv.service` saw
  `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE` in the manager's environment, and stops it
  at logout, so a re-login with the manager still up (lingering, another session) runs it again
  (the pattern of Omarchy's own `omarchy-migrate-notify.service`). `default.target` would not:
  with a lingering manager it stays up across logins. Sessions started without uwsm never reach
  the target; there the next Tuner start (or `tuner --refresh`) arms, as before.
* **Environment**: the service's environment is the manager's, so every session variable it
  has is one the manager has: nothing is passed with `--setenv` and the units read the
  manager's values when they fire (a run before the variables arrived would have none to pass
  either). A unit armed at login therefore fires into the session that is current at its time.
* **Safe to run any time**: no database yet (an account that never started Tuner) → exit 0
  having made nothing, not even the log; Tuner or a refresh holding the instance lock → exit 0
  (they arm themselves); the lock is held while arming, so a window started meanwhile waits a
  moment; setting off, a `--profile-dir` profile or no `systemd-run` → exit 0; systemd refusing
  → exit 1 (the unit shows failed). Twice in a row is the same as once: a fresh `ArmSync`
  learns the armed units and re-arms them for the times planned now. A store migration after
  an upgrade happens here, under the lock, as for `--reminder`.
* **Due while nothing ran**: a reminder whose unit time passed during the reboot or logout
  (or a switch whose unit fired with no display) is what a starting Tuner fires late — a note
  while its programme airs (notes on), a switch up to `LATE_SWITCH_S` after its time.
  `closed_plan` gives each such one a unit `LATE_UNIT_S` (5 s) from now, ahead of the others
  within the 30, so `tuner --reminder` fires it late by the same rules; past those it is
  dismissed by the next start, as before. Its `--reminder` waits for the lock if the re-arm
  still holds it (a few seconds; it waits up to 10).
* **A time that passes while arming**: `SystemdArmer::arm` arms no earlier than 2 s from the
  call — an `OnCalendar=` time already past never elapses — so a unit due during a slow run
  (a re-login re-arms every unit, five `systemctl` calls each) fires a moment late instead of
  never.
* Suspend and resume need nothing: the user manager, and its timers, stay up.

### 36.3 Sleep timer
Ctrl+Shift+T, the player menu, the status-bar chip ("Sleep timer — 23:10 (Ctrl+Shift+T)" with
✕) and Settings ▸ Reminders: stop or pause, in 15 min–2 h, at the end of this programme, after N
items, or at the end of Up next. An item counts once when it ends (an ended item that stays on
screen is not counted again) or when the next starts within 90 s of its end; live is never an
item. Between two Up next items (the card, a PIN wait, a future slot) Up next hasn't run out.
It warns `reminders.sleep_warn_s` before, then stops or pauses every picture; a news flash on
ends with it and the window pops back in. It runs on tokio, so it fires with the window hidden.

### 36.4 Focus on the scheduled auto-launch
Closed in v0.2: the scheduled auto-launch (Tuner closed, a switch due, Open in: Pop-out) starts
Tuner with the app id `tuner-popout`; the packaged Hyprland rule maps that window floating,
pinned and with `no_initial_focus` (the webcam-overlay pattern), and it pops out at once
(§21.6). It needs the snippet installed: without it the window maps tiled and may take focus.
Verified against Hyprland's source and docs only; the real-desktop check is in
`docs/MANUAL_CHECKLIST.md` §27.

## 37. Use Tuner from Claude: the plugin, skills and pass-2 tools

Build notes: `docs/design/agent.md`. Extends §30.

### 37.1 Tools
30 tools (casting adds `cast_devices`, §30.5; Watchlists adds `watchlist`, §42): the 13 of §30.5 (extended: `guide_search` without a query over a window and
categories, `whats_on` filters, `details.copies`, `search.explain_filtered`, `status.queue /
multiview / streams / sleep_timer`, `control.previous_channel / popout / tile`), plus
`match_titles`, `history`, `episodes`, `favourites_edit`, `parental_status`, `lock`,
`diagnose`, `sources`, `reminders`, `sleep_timer`, `queue`, `follow`, `spoiler_guard`,
`guide_fix`, `multiview`, `watchlist`. A tool whose feature's kill switch is off answers `unavailable`.
Every read reply passes the spoiler guard (`spoiler_safe` strips harder; `reveal_spoilers`
names what may show).

### 37.2 Guardrails added
Locks only add (`lock session_now` locks the session in the app); locked items are never added
to favourites or Up next; explicit items while hidden are `not_found` everywhere, counts
included; `reminders add` finds the programme's row in any state (a switch asked for after the
notification fired still switches) and refuses hidden channels; sleep-timer times in the past
are refused; Following, spoiler and guide-fix limits as in §31.5, §33.5 and §34.4. No tool
returns a log line or a URL.

### 37.3 The plugin
* `.claude-plugin/marketplace.json` (repo root) → `plugins/omatuner`: `plugin.json`, `.mcp.json`
  (`tuner mcp`), `RULES.md` (the shared rules every skill reads first, via
  `${CLAUDE_SKILL_DIR}/../../RULES.md`) and seven skills: **watch**, **pick**, **series**,
  **plan** (reminders, switches, news flashes, sleep timer), **kids**, **fix** (guide fixes,
  playback trouble) and **discover** (`match_titles` against what's trending).
* Install: `claude plugin marketplace add rthoms334/omatuner` then `claude plugin install
  omatuner@omatuner`. Without the plugin: `claude mcp add tuner -- tuner mcp` (tools only).
  The Arch package ships the marketplace under `/usr/share/tuner/claude/`; `tuner claude-setup`
  prints the steps, `--apply` runs them after a "y" on the terminal. Since §39 Tuner runs the
  same steps itself (the first-start card, Settings ▸ AI agents) and keeps the installed plugin's
  version in step with Tuner's.
* `claude plugin validate ./plugins/omatuner --strict` and `claude plugin validate .` pass; a
  test checks every `allowed-tools` name is a real tool and that the plugin's version follows
  the app's.
* The plugin is Claude Code only: the skills read `${CLAUDE_SKILL_DIR}` and list Claude Code's
  plugin tool names. Other agents use `tuner mcp` directly (README "Use Tuner from an AI
  agent") and get the tools, their descriptions and `tools::INSTRUCTIONS`, without the skills.

## 38. Playlist codes and a faster library load

* **Playlist codes** (§13.0): every playlist has a 1–3 letter code (derived, or the user's in
  the playlist editor) shown as a chip on cards and rows with two or more playlists. Derived
  codes are assigned in creation order, so reordering playlists never swaps them; a user's code
  always wins.
* **Library load** (`store::library`, build notes `docs/design/perf.md`): item tables are read
  in id order with no SQL sort or override join, display order is rebuilt in memory, group links
  merge through a cursor, hidden titles are dropped before the rest of the row is read. On the
  synthetic catalogue (80k channels, 300k movies, 40k series, 2.5k categories) the default rule
  loads in about 350 ms (was 650 ms); `benches/startup.rs` gates it under 550 ms and the rule's
  cost at 1.25× the rule with titles kept. Builds of 1 s or more log their steps at info.

## 39. Connecting AI agents (the card and Settings ▸ AI agents)

CLI behaviour verified 2026-10-12 against claude 2.1.296, codex-cli 0.162.1, gemini 0.63.0,
Copilot CLI 1.0.95, cursor-agent 2026.10.01 and opencode 1.18.35, every mutating command in a
throwaway `HOME`. Extends §30 and §37.

**Problem (user report, 0.2.2).** After installing the package, Claude Code knew nothing about
Tuner: the plugin under `/usr/share/tuner/claude` is opt-in and nothing said so. **User decision
(2026-10-12):** "build the first-run card and connect it now" and "We should also have a way to
do it in settings also".

### 39.1 Decisions
1. **The package never connects anything.** pacman runs scriptlets as root, an agent's settings
   live in the user's home (Arch packages must not write to `/home`), and they belong to another
   app. The package only ships the plugin and echoes a hint (§39.8); Tuner, running as the user,
   does it when the user says so.
2. **Six agents known, five automated.** Claude Code gets the **plugin** (tools + 7 skills);
   Codex, Gemini CLI, Copilot CLI and opencode get **plain MCP** (`tuner mcp`): the skills use
   Claude Code's plugin tool names and `${CLAUDE_SKILL_DIR}`, and their plugin routes are
   untested in real sessions. opencode has no remove command, so its Disconnect is by hand.
   Cursor is copy-and-paste only (no `mcp add`; the user approves servers per workspace in
   Cursor anyway). VS Code, Zed and others get an *Other agents* note with Copy actions.
3. **Only the agents' own CLIs write their settings.** Tuner reads status (the `tuner` entry
   only) but never writes another app's config file: comments and formats are the CLI's
   business, and a CLI that changes its format keeps working.
4. **Status reads never start MCP servers.** `claude mcp list/get`, `gemini mcp list`,
   `cursor-agent mcp list` and `opencode mcp list` health-check by spawning every configured
   server (including `tuner mcp` and the user's others), so they are never called; Tuner uses
   `claude plugin list --json`, `codex|copilot mcp get --json tuner` or the config files.
5. **One card per Tuner version, no nagging.** It shows only when a found, automatable agent is
   not connected; *Not now* waits for the next upgrade; *Don't ask again* is the same switch
   as the Settings toggle (`offer_connect = false`), so the user can see why it stopped.
6. **Keeping the plugin in sync is automatic** (`keep_in_sync`, on): the user opted in by
   connecting, Claude Code caches the plugin by version, and the update is cosmetic for the
   packaged folder (it loads in place) but needed for a GitHub-sourced install. A toast says it.
7. **No new key bindings.** The card is a `Modal`, the page a `Panel` with the row menu, so the
   README key tables and `?` help are unchanged.

### 39.2 Config (`[agents]`, `config::AgentsConfig`)
`offer_connect` (true), `keep_in_sync` (true), `checked_version: Option<String>` (the Tuner
version whose start check finished: the card was answered, or there was nothing to offer;
`None` on a new install or an upgrade from 0.2.2 or older), `play_in` (`pop-out`; where an
agent's play opens, §30.13). `connect::should_check` = (either
switch on) and `checked_version` ≠ this version. Machine-local: `Config::restored` keeps this
machine's `[agents]` from a restored backup (connections are per computer).

### 39.3 The service (`agent::connect`, `Services::agent_connect`)
`AgentConnect` owns detection, status, operations, the start-of-version pass and "last used".
The UI reads `rows()` snapshots (`AgentRow { id, status, busy, last, last_used }`) each frame;
`refresh`, `refresh_if_stale` (30 s; the page's `enter`) and `run(ids, op)` work in the
background and request a repaint on every change. `run` has **one queue**, so two CLI changes
never overlap; each result is toasted and that agent's status read again. `start_pass` (≥ 3 s
after start) refreshes, runs Claude's Update when `keep_in_sync` and the plugin is
behind, then `decide`s: `Offer` (found, automatable, not connected; `AgentId` order, the first
ticked; plus `connected`, the agents found already connected or with an older plugin, for the
card's "already connected" lines), `Settled` (agents found but nothing to offer, or
`offer_connect` off: record `checked_version` now) or `Retry` (nothing found: record nothing,
the next start looks again). `shutdown` kills running children before the runtime stops. A
refresh's `refreshing` flag is cleared by a drop guard (a task dropped at shutdown or a panic
can't wedge it), and the pass waits at most 60 s for a refresh it didn't start.

**Only on the user's own desktop.** `for_services(paths, …)` uses the `NullRunner` (and never
starts the pass) off the desktop (`platform::off_desktop`: unit tests, `keep_off_desktop`,
`TUNER_NO_DESKTOP`) or when `AppPaths::home` isn't `$HOME` (a test profile with a fake home).
The integration harnesses (`agent_e2e`, `e2e_flows`, `e2e_soak`) call `keep_off_desktop` too.

Statuses: Checking, Not found, Not connected, Connected (plugin + version, MCP only, or MCP),
Update available (Claude: `version` older than `folderVersion`, or a GitHub install older than
Tuner — until Claude's own update calls that version up to date, which the service remembers
for the session), Turned off (`enabled: false`), Needs repair (plugin entry with `errors`, e.g. the package's
folder is gone; Repair = Connect), Couldn't check (redacted first line, ≤ 120 chars).

### 39.4 Running agent CLIs (`connect::runner`)
`CommandRunner` (`which`, `run`) behind a trait: `SystemRunner` in production, `NullRunner`
off the desktop (tests, `TUNER_NO_DESKTOP`: spawns nothing), `FakeRunner` (cfg(test), scripted
answers, records every argv). `SystemRunner` rules:
* argv, no shell; stdin closed; cwd `$HOME` (no project scopes or folder-trust prompts);
  inherited env plus `GIT_TERMINAL_PROMPT=0`, `NO_COLOR=1`, Copilot `COPILOT_AUTO_UPDATE=false`;
  `PATH` plus `~/.local/bin`, mise's shims, `~/.bun/bin`, `~/.npm-global/bin`, `~/.volta/bin`
  (a launcher-started Tuner may lack the shell's PATH).
* Timeouts 15 s (status) and 120 s (changes; Claude's own clone timeout); stdout and stderr
  drained on threads (capped at 1 MiB each, no pipe deadlock); its own process group, killed
  whole (node and git grandchildren too) on timeout or shutdown, and after a normal exit when
  something it left running still holds the output open 2 s later (so no reader thread stays).
* Logged at info for changes: the command line and `ok` / `exit N` with the redacted first line
  of the result; status reads only their outcome at debug. Never the raw output of a list or get
  command, nor any config file (other servers' secrets, `~/.claude.json` account data).
* Never `gemini extensions install` (hangs on a trust prompt even with stdin closed) or
  `--trust` on `gemini mcp add` (it skips tool confirmations).

### 39.5 Per agent
| Agent | Status | Connect | Update | Disconnect |
|---|---|---|---|---|
| Claude Code | `claude plugin list --json` → `omatuner@omatuner` in the user scope (no `scope`: an older CLI, user); none → `~/.claude.json` (`$CLAUDE_CONFIG_DIR`) `mcpServers.tuner` or a project's `projects.*.mcpServers.tuner` = MCP only | `plugin marketplace add --json <share dir, else rthoms334/omatuner>`, `plugin install --json omatuner@omatuner --scope user` | `plugin marketplace update --json omatuner`, `plugin update --json omatuner@omatuner` | `plugin uninstall --json omatuner@omatuner --scope user` (`not_installed` = ok), `plugin marketplace remove --json omatuner` (`not_configured` = ok) |
| Codex | `codex mcp get --json tuner` | `codex mcp add tuner -- tuner mcp` | — | `codex mcp remove tuner` |
| Gemini CLI | `~/.gemini/settings.json` (JSONC) `mcpServers.tuner` | `gemini mcp add -s user tuner tuner mcp` | — | `gemini mcp remove -s user tuner` |
| Copilot CLI | `copilot mcp get --json tuner` (`Server "tuner" not found` = not connected) | `copilot mcp add tuner -- tuner mcp` (`already exists` = ok) | — | `copilot mcp remove tuner` (`Server "tuner" not found` = ok) |
| opencode | `$XDG_CONFIG_HOME/opencode/opencode.json[c]` (JSONC) `mcp.tuner` | `opencode mcp add tuner -- tuner mcp` | — | by hand (copy where) |
| Cursor | `~/.cursor/mcp.json` `mcpServers.tuner` (found: `cursor-agent` or `~/.cursor/`) | by hand (Copy JSON) | — | by hand |

Claude's steps stop at the first failure; re-running Connect migrates a GitHub marketplace to
the packaged folder. A Claude MCP-only setup is never removed automatically (the menu copies
`claude mcp remove --scope user tuner`). File-status agents run `<cli> --version` once per
refresh as the liveness check. Config files are read with a 32 MiB cap through a small JSONC
reader (comments, trailing commas).

### 39.6 The first-run card (`ui::overlays::agent_connect`)
App start: `should_check` → `start_pass`; each frame `take_pass_outcome`. The card opens only
when `agent_card_may_show` holds: onboarding done, no other overlay, not on the Player, not full
screen, not popped out or video-only, not multi-view, no text field focused. A preview playing
in a browse screen doesn't block it, nor does Settings ▸ AI agents (the card would repeat it).
When the gate opens, the offer is checked again: with `offer_connect` turned off meanwhile, or
no offered agent still *Not connected* and idle, nothing opens and `checked_version` is
recorded (agents connected meanwhile join the "already connected" lines). If the gate never
opens, nothing is recorded and the next start checks again. One agent: "Connect Claude Code to
Tuner?" with what it adds; several: a tick list (only the first ticked) and "Connect N agents".
Above the offer, each agent already connected (`Offer::connected`) gets a plain line,
"✓ Claude Code  already connected" (the words carry it; the muted colour doesn't have to), so
an agent the card doesn't offer doesn't look missing; the lines take no focus, ticks or clicks.
Enter Connect, Esc Not now, Tab/arrows move, Space ticks; mouse buttons, a scrim click = Not
now. The card opens by itself, so for its first 0.6 s Enter, Space and clicks do nothing (a key
meant for the list behind can't connect an agent); Esc works at once. Connect runs
`run(ticked, Connect)` and keeps the card with a progress line per agent until every one is
idle (Esc hides it early; the work continues, toasts carry the results). Both answers set
`checked_version`; *Don't ask again* also sets `offer_connect = false`.

### 39.7 Settings ▸ AI agents (`ui::screens::settings::agents`)
After Parental. The two toggles, *Agents play in* (Pop-out / Tuner window, §30.13; off Hyprland a
note says agents play in Tuner's window there), *Check again*, one row per agent found (status, "used 2 h ago"),
"Not found on this computer: …", and *Other agents* (Copy `tuner mcp`, the VS Code command, the
README link). Enter or a click opens the row's menu: Connect / Repair, Update to <version>,
Disconnect… (destructive confirm naming what the agent loses), Copy the setup command, the
by-hand copies (Cursor JSON and path, opencode's removal, Claude's MCP-only removal) and the
README link. A busy row has no menu. A failed op shows under its row until that agent's next
op (a refresh keeps it); a *Couldn't check* status is repeated in full under its row. With no
agent found, one note names them all; the plugin switch hides while Claude Code isn't found.
Copy goes through egui's `copy_text` and toasts "Copied"; there is no URL opener.

**Last used by.** `AgentHandler::handle` notes each named agent (`AgentName::stored()`; unknown
names are not recorded) in the database's `meta` table, key `agent.used.<Name>` = unix seconds,
throttled in memory to once a minute per name, written fire-and-forget through the store's
writer; only the newest 20 names are kept (the name comes from the client). The page's intro says "Last used by Claude, 2 h ago" (newest of all names, Zed
included); a row whose friendly name matches shows "used …".

### 39.8 The pacman message (`packaging/omatuner.install`)
`post_install` echoes two lines (Tuner offers it when it next starts, or Settings ▸ AI agents;
Claude Code: `tuner claude-setup`). `post_upgrade` echoes them only when coming from a version
older than 0.2.3, the release that brought the card, so later upgrades stay quiet. Nothing else
runs as root.

### 39.9 Tests
No test spawns an agent CLI. Unit: Claude list parsing (current, GitHub source, downgrade,
disabled, errors and `errors: null`, scopes, MCP only incl. a project's, `$CLAUDE_CONFIG_DIR`,
leading warnings, missing fields, garbage); Copilot's own "not found" vs a wrapper's; step results
and the "already done" rules; JSONC and file status; exact argv, env and timeouts per agent and
op; `should_check`, `decide`, `version_lt`; the service with `FakeRunner` (order, stop at the
first failure, sync on start, the queue); `SystemRunner` with real shell utilities only
(timeout kills the group, 2 MiB output, stdin closed, missing program); usage throttling; the
config restore. kittest with the fake: the page (rows, Connect, Disconnect confirm and cancel,
Copy), the card (Enter, Esc, Don't ask again, several agents, a key or click as it appears),
and the gate (waits for the player to close; a stale offer isn't shown).

---

## 40. Casting to a TV (AirPlay to an Apple TV, Phase 1)

The full design, with its reasoning, the pinned reference implementations and the manual
checklist, is `docs/design/cast-design.md` (it stays the authority for the protocol and the
decision records). **Section numbers written as "DESIGN §40 §x.y" in the code are that file's
§x.y.** This section says what shipped and where; the sections it touches (§11.5, §13.8, §15.2,
§20, §21.3, §22, §30) carry a pointer back here. User-facing text: README "Casting to a TV";
checks on a real TV: `docs/MANUAL_CHECKLIST.md` §30.

### 40.1 What it is
Press `C` in the player and pick a TV, choose *Cast to…* on any channel, card, programme or
episode, or set *Play on* to a TV (§40.3): picture A's controller sends what plays to the TV
instead of mpv. Tuner is the relay: it pulls the stream from the provider (one upstream
connection, as the local player would), repackages it with `ffmpeg` into HLS and serves that to
the TV over the LAN; the TV never sees a provider address or login. Phase 1 casts to an Apple TV
4K (tvOS 26.x and 27, pairing mandatory) over AirPlay 2 video, in native Rust with no helper
process. Third-party AirPlay TVs (the Hisense) are listed and offered like the Apple TV and cast
with VioletRelay's third-party profile; there is no experimental tier. Chromecast, DLNA
and Roku are later phases (cast design §15.3). The whole feature sits behind the compile-time
switch `cast::ENABLED` (`src/cast/mod.rs`; the key table's `feature_on`, the service start, the
OSD and menu items, the Settings page, the cards and the agent's tools all read it) and the
user's `[cast] enabled`.

### 40.2 Pieces (cast design §5.3 is the map; what exists)
* **Value types and the seam.** `model/cast.rs` (ids, kinds, phases, `CastView`, `CastErrorKind`,
  `CastHints`) is shared; `player/remote.rs` defines the controller's contract (`CastConnector`,
  `RemoteOutput`, `RemoteCmd`, `RemoteEvent`) and the pure `RemoteEvent` → `state::Input`
  adapter, so `player` never depends on `cast`. The controller treats a cast like an output
  that is not mpv: zaps, reconnects, seeks, Up next, progress and the failure cache run through
  the same state machine (§11). Live preview autoplay is off while casting.
* **AirPlay client** (`cast/airplay/`): TLV8, SRP-6a, HAP framing, pair-setup (HKP 3, HKP 5
  fallback only inside the pairing session) and pair-verify, binary plists, the HTTP/RTSP
  transport on one connection, SETUP / type-130 video, the `/command` play queue (insert,
  replace, set rate, seek), the event channel, `playbackInfo` positions, and the redacted
  `TUNER_CAST_TRACE` transcript. Ported from pyatv (MIT) and the pinned references of cast
  design §7.0, with the notice in `THIRD_PARTY_NOTICES.md`. Timing is declared PTP, never NTP.
  tvOS 27 differences (client name, `skipRecord`, split event frames) are behaviour, not
  branches (cast design §7.6).
* **Relay** (`cast/relay/`): a loopback upstream proxy (`upstream.rs`: one provider connection,
  playlist rewriting, `Range`, a film's broken stream resumed from the byte it reached, the
  cause of a break logged), `probe.rs` (ffprobe), the pure ladder `plan.rs` (L1 original, L2
  repackage, L3 convert with libx264; DRM refused; interlaced is deinterlaced for AirPlay,
  Apple's HLS rule 1.14; multichannel sound on the Apple TV adds a surround rendition), the
  ffmpeg supervisor (`ffmpeg.rs`; films and catch-up read a 60 s burst, then real time), the
  served-playlist guard and playlist views (`playlist.rs`: a target duration pinned per item
  and never changed mid-stream, `EXT-X-START` at 0; seeks inside what is produced cost no new
  upstream), the surround multivariant playlist (`renditions.rs`: a stereo AAC group and a
  surround group, Apple's authoring spec 2.3, 9.6), the LAN server (`server.rs`, axum: only
  `GET`/`HEAD` of `/c/<token>/[v|a|s/]<file>`, bound to the one address that reaches the TV,
  peer check, listening only during a session) and tokens (`token.rs`: 128 random bits, one per
  load generation, `Secret`, revoked on stop). Live output goes to tmpfs (`cast_live_dir`),
  films and catch-up to `cast_vod_dir` (8 GiB cap, production paused 180 s ahead of the TV,
  wiped on stop, swept at startup).
* **Service** (`cast/service.rs`, `session.rs`, `discovery.rs`, `registry.rs`, `inhibit.rs`,
  `store/cast.rs`): started and stopped by `Services` like the MPRIS service; mDNS discovery
  (`mdns-sd`) with a manual address probe; one session task per cast driving receiver and relay,
  escalating L2 → L3 once (the result kept in `cast_hint`; a surround item the TV fails goes
  again in stereo first, a segment longer than the pinned target restarts at the same level),
  the 10 s "TV never fetched" firewall check, one `playbackInfo` every 10 s (tvOS 27 answers
  under `info` with CMTimes; else an estimate from `timeJumped` and the TV's fetches, 2 s
  behind), every reload continuing where the TV is, the logind sleep inhibitor
  (`cast.keep_awake`), and INFO lines for a session's and an item's end with why. Pairing
  identity and devices live in `cast_identity` / `cast_device` (migration `0014_cast.sql`,
  never exported by backup).
* **Firewall** (`cast/firewall/`, `src/bin/tuner-firewall.rs`, `packaging/polkit/`): detection
  without root from `/etc/ufw/*` and `systemctl is-active` (`FirewallState`), made toward one
  address and cached per address (a TV is judged only toward its own), the `user.rules`
  parser, and the *Allow on firewall* / *Remove rule* runner through `pkexec`. The helper
  validates its verb, interface (present in `/sys/class/net`), RFC 1918 CIDR and port, refuses
  unless root under pkexec, and runs one fixed `ufw` command tagged `omatuner cast`. From a
  checkout it falls back to the generic polkit action running `ufw` itself.
* **UI** (`ui/overlays/{cast_picker,cast_pair,cast_firewall,cast_offer}.rs`,
  `ui/screens/settings/cast.rs`, the player's OSD / banner / cards and `video_host.rs`): the
  picker (`C`, the Cast control, the menu), the PIN overlay (600 ms arming guard, the PIN used
  once and never logged), the casting card (phase line, *Stop casting*, *Play here*, error
  actions), the first-run discovery card and three-step wizard (pair, network, Test on TV),
  Settings ▸ Cast, a compact-layout status chip, and `cast_offer::error_sentence` as the one
  place the §3.4 failure sentences are written. `ui/casting.rs` is casting from where things
  are chosen: *Cast to…* on items (through `ui::item_menu`, `UiCtx::cast_with`) and the Play on
  switch's chooser and status chip; the picker has three purposes (what plays, an item, Play
  on).
* **Agents** (§30, `agent/`): `cast_devices`, `play {device}`, `control stop_casting` /
  `play_here`, `status.casting`, `diagnose.cast`, the `RULES.md` section 12 and the `fix`,
  `watch`, `kids` notes. "Put it on the TV" counts only paired TVs: one is cast to without asking
  (the request is the yes), two or more are asked which (§30.5).

### 40.3 Behaviour worth knowing
* **Cast to… on what you pick** (cast design §3.1; 0.3.0 field feedback: casting needed playing
  locally first). The item menu of a Live TV row, a Home tile, a card, a title's page, an episode
  and a guide programme has *Cast to Living Room* (one TV ready) or *Cast to…* (the picker for
  that item), and `C` / Alt+C does the same on the focused one. The item plays exactly as
  Shift+Enter plays it here — PIN, copy and fallback, Resume / Start over — with its commands
  turned into `CastTo` / `CastCopy` (`UiCtx::cast_with`); nothing plays here first, the screen
  stays, a toast says "Casting BBC One to Living Room". A programme casts its channel while it
  airs, its catch-up once aired; an upcoming one isn't offered.
* **Play on** (cast design §3.9). A switch picture A's controller holds
  (`PlayerCommand::SetPlayOn`, `PlayerSnapshot::play_on`): while it names a TV, a `Play` /
  `PlayCopy` that would start here is cast there instead, whoever sent it (the screens, Up
  next, reminders, `--play`, an agent's `play` without `device`, answered `opened_in: device`
  with `status.play_on`), except the Live preview's (and the preview autoplay is off) and in
  multi-view. Chosen from the status chip *Play on: … ▾*, Alt+T or Settings ▸ Cast; a choice
  moves what plays (a speaker picker). Only the user changes it: failures, Stop casting and Play
  here leave it. `cast.play_on` keeps the last choice; it comes back at start only with
  *Remember Play on across restarts* (off). Reminder switches and agent plays on it leave the
  window in place, as casts do (§3.8).
* **`CastCopy`**: a title's copy cast with its fallback and start point (the UI's *Cast to…* of
  a film or an episode, and an agent's `play {device}` of one, which lost both in 0.3.0).
* **Stream info while casting** (cast design §11.3): the relay reports what the TV is fed at
  every launch, probed or not (the probe is skipped whenever the hints suffice, which left the
  0.3.0 panel on "Reading stream info…"): `StreamInfo::cast` (`CastFeed`). The panel shows the TV
  and its model, the source's codecs, size, rate, scan and sound layout, each stream's way to the
  TV (copied, converted, deinterlaced; AAC stereo; surround copied or AC-3 5.1), the segments and
  their pinned target, behind-live, and whether positions are the TV's `playbackInfo` or the
  estimate (`CastApi::position_source`).
* **Where alerts go** (cast design §3.8). Casting never moves the window: an agent's cast, a
  reminder's switch while casting and a play from anywhere leave the workspace alone (a locked
  item still raises Tuner for the PIN). A cast that starts while the pop-out is up pops it back
  in, unraised. A failure shows on the casting card when the player is on screen, as a toast when
  the window is seen, and as a desktop notification with **Show** (`PlatformEvent::ShowPlayer`)
  when it is not.
* **Same generation reloads.** Escalation, Convert, a film seek past what is produced and a live
  resume close and reopen the provider under one generation; the session holds the close back so
  the controller does not read it as the upstream dropping (and still reports it when the
  reload fails or is superseded).
* **Catch-up seeks clamp** to the produced edge (`RemoteEvent::SeekClamped`, toast "Catch-up can
  only seek as far as it has loaded"); films restart ffmpeg with `-ss`.
* **Config** `[cast]`: `enabled`, `offer_setup`, `checked_version`, `default_device`,
  `media_port` (47800), `min_segments` (2), `convert` (auto | never), `interlaced` (auto |
  as-is), `keep_awake`. Machine-local like `[agents]`. The test builds' `experimental` is gone
  (an old key is ignored).
* **Accepted limits.** Alert toasts carry one button (the `Action` enum has no Play here / Cast
  again / open-firewall-card actions; the menu uses `Action::Digit(248/249)`); *Show command*
  opens the firewall card, where it is pressed again; the *Pairing refused* sentence merges the
  470 hint and the "Everyone in your Home" hint; separate HLS audio tracks are not followed (such
  a provider plays without sound); moving the cast to another TV that then fails to open leaves
  the player Idle; `cast_devices` is listed even while casting is switched off. The C key in
  lists and grids is Alt+C (a plain `C` types into the filter there). The position source is
  one process-wide slot (one cast session at a time). While the Compact cast chip shows, the
  Play on chip doesn't (Alt+T still opens the chooser).

### 40.4 Tests (cast design §14)
Pure: TLV8/SRP/HAP vectors from pyatv 0.18.0 itself (`tests/fixtures/cast/vectors/`), the plan
ladder, the playlist guard, the ufw parser (fixture `user.rules` files), the helper's argument
validation, the remote-event adapter. Scenario: `tests/cast_airplay.rs` against the fake
receiver (`tests/support/fake_airplay.rs`, tvOS 26 and 27 modes), `tests/cast_relay.rs` over
loopback, `tests/cast_service.rs` (fake receiver and relay), `tests/cast_firewall.rs` (a fake
`pkexec`), the controller's casting section in `tests/player_mpv.rs`, the agent's in
`tests/agent_app.rs`, and kittest screens in the modules. Cast to… and Play on: the controller's
diversion (every play but the Live preview's, Play here keeping the switch), `CastCopy`'s fallback
and start point and the never-opened session's Play here (`tests/player_mpv.rs`); Alt+C on a
Live TV row going straight to the one TV, the chip and the chooser sending Shift+Enter to the
TV, and the switch restored only when remembered (app rig); the picker's item and Play on
modes, `cast_with` (the screen stays, the Resume prompt carries the TV), the keys (`C` never
taken from a filter); `status.play_on` and a `play` without `device` on the switch's TV
(`tests/agent_app.rs`); the feed's mapping per rung of the ladder (`relay/plan.rs`) and the
info panel while casting (`banner.rs`). Two `#[ignore]`d tests interoperate with
a real pyatv (`TUNER_CAST_PYATV`) and the LAN (`TUNER_CAST_LIVE`). Not covered: the fMP4 keyframe
check end to end, the 8 GiB film cap, `CpuTooSlow`, and pkexec, polkit and ufw on a real system
(manual checklist §30 item 15).

---

## 41. Tuner's updates (checked, signed, installed, restarted when idle)

Code: `src/update/` (`verify`, `github`, `service`, `helper`, `restart`),
`src/bin/tuner-update.rs`, `src/app/update.rs`, `src/ui/screens/settings/updates.rs`,
`platform::hypr::{placement, put_back}`. Security: §20.4. Packaging and signing: §22.

### 41.1 Decisions (user, 2026-10-13, authoritative)
1. **Install: automatic, with the password; passwordless opt-in.** Tuner downloads and verifies
   by itself, then installs through pkexec with the system's password dialog (`auth_admin_keep`).
   Settings' *Install updates without asking for my password* writes, through the helper under
   pkexec once, a polkit rule that allows only the install action for the active local user;
   turning it off removes the rule. The helper is built to be safe without a password (§20.4).
2. **Restart: when idle.** After an install while Tuner runs: the chip "Updated to X — restarts
   when idle (Restart now)"; Tuner restarts by itself when nothing plays (or casts), nothing is
   open and the window is away, after a quiet period — never during playback.
3. **Channel: stable releases only**, at start (after a short delay) and every 6 h, through the
   GitHub Releases API with the app's HTTP client and proxy settings, conditional requests, no
   token.
4. **Signing: minisign.** The maintainer keeps the secret key (repository secrets
   `MINISIGN_SECRET_KEY`, `MINISIGN_PASSWORD`); the public key `packaging/update-key.pub` is
   compiled in. A placeholder key installs nothing. Verification: `minisign-verify` 0.3.0 (MIT,
   no dependencies, 18 days old at the pin), not aws-lc-rs: minisign's pre-hash is BLAKE2b,
   which neither aws-lc-rs nor ring offers.
5. **In-app first:** Settings ▸ About & logs ▸ Updates, notifications and toasts. A Tuner not
   installed with pacman checks and tells only, and says why.

### 41.2 Checks (`update::github`, `Updater::check`)
`GET https://api.github.com/repos/rthoms334/omatuner/releases?per_page=20` with
`Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`, `User-Agent:
Tuner/<version> (+repo)` and `If-None-Match` from the last 200 (`HttpClient::get_conditional`;
answer and ETag kept in `~/.cache/tuner/update/releases.{json,etag}`; a 304 doesn't count against
the 60/h limit). 403/429 with `X-RateLimit-Remaining: 0` or `Retry-After`: the next check at the
reset (1 min–24 h), a quiet note in Settings, no toast. Other failures: again in 1 h. `pick`:
not draft, not pre-release, tag exactly `v<n>.<n>.<n>`; the newest by `vercmp`; its package for
`std::env::consts::ARCH` (`omatuner-<ver>-<rel>-<arch>.pkg.tar.zst`, version = the tag's) and
that file's `.minisig` — both, or the release is only told about. Newer means: than pacman's
installed `omatuner` (full version) when pacman manages Tuner, else than the running version.
The loop sleeps in ≤ 10 min chunks against the wall clock (a laptop that slept checks late, not
never); *Check now* and any `[updates]` change wake it.

### 41.3 The key and the signatures (`update::verify`, shared with the helper)
`RELEASE_KEY = include_str!("packaging/update-key.pub")` (minisign's two-line `.pub`; key id
7FC95FA61A0134E8). `release_key` refuses the placeholder (`REPLACE-WITH-THE-RELEASE-KEY`) and
anything that doesn't parse. `verify` accepts only pre-hashed signatures (`ED`: Ed25519 over
BLAKE2b-512 of the file, minisign's default), by the key's id, with a valid global signature
over the trusted comment; legacy (`Ed`) signatures are refused (release.yml never makes them).
`vercmp` is libalpm's `alpm_pkg_vercmp` (epoch, `rpmvercmp`, release only when both have one),
tested against pacman's own table. The same file is compiled into the helper (`#[path]`), so
the app and the helper can't disagree.

### 41.4 The service (`update::service::Updater`, in `Services`)
```text
start ─20 s─▶ check ◀─ every 6 h · Check now · [updates] changed · 1 h after a failure · the rate-limit reset
               │ Mode: this exe is /usr/bin/tuner, pacman -Qqo says omatuner, helper + policy + pkexec,
               │       a real key → Managed{installed}; else TellOnly(why)
               ├─ pacman's omatuner newer than what runs ─────────────────────────────▶ Installed
               ├─ releases (If-None-Match) ─ newest stable not newer ─▶ UpToDate
               ├─ TellOnly, or automatic installs off ─▶ Available: notification + toast, once per version
               ▼
          Downloading: <pkg>.minisig, then <pkg> → ~/.cache/tuner/update/ (0700; files 0600)
               │ verified here ─ fails ─▶ deleted, refused this session, error toast, Available
               ▼
             Ready ── passwordless rule on (held while a TV pairs) ──────┐
               │ notification "ready"; the app's gate: window focused,   │
               │ nothing plays or casts, nothing open, not typing, not   │
               │ popped out                                              │
               ▼ (toast: "Installing Tuner X: enter your password")      ▼
          Installing: pkexec --disable-internal-agent /usr/lib/tuner/tuner-update install <pkg>
               │ 126 dismissed → Ready (declined: Install now) · 127 → Ready + why
               │ 3 refused → Available, not again this session · 4 failed → Ready + pacman's message
               │ 5 busy → Ready, next check in 10 min · 6 no key → TellOnly
               ▼ 0: downloads removed
          Installed: toast "Tuner updated to X — it restarts when idle" [Restart now], notification,
               │     status-bar chip "Updated to X — restarts when idle (Restart now)"
               │ every 5 s, held 60 s: A and B idle (not even paused), no multi-view, no overlay /
               │ typing / onboarding, no job, not installing, nothing casting, the window without
               │ the keyboard
               ▼
          restart (§41.6) ─▶ the new process: marker → toast, window put back, §39 pass
```
The UI reads `status()` (phase, mode, last check, problem, the switch) each frame and calls
`check_now`, `install_now` (Settings, the toast's *Install*, the app's prompt gate),
`set_passwordless`, `restart_now`; the app tells it `note_ui` every frame and saves
`take_config_change` (the switch, once the helper succeeded) into `[updates]`. Notifications go
out on the blocking pool (`platform::notify::notify`). `shutdown` stops the waits; an install in
pacman's hands finishes (it runs as root and can't be killed from here). Off the desktop
(`platform::off_desktop`) or with a profile whose home isn't `$HOME`, nothing runs
(`status().inert`).

### 41.5 The helper (`src/bin/tuner-update.rs`) and polkit
`tuner-update install <path>` and `tuner-update passwordless on|off`, as root through pkexec
only (`PKEXEC_UID` set). `install` runs the install as the system service
`omatuner-update.service` (`tuner-update install-as <uid> <path>` under `systemd-run`, §20.4:
nothing of the caller's process reaches pacman, one install at a time). The checks, in order,
are §20.4's; the passwordless rule is
`/etc/polkit-1/rules.d/49-omatuner-update-<uid>.rules` (0644, written atomically):
```js
polkit.addRule(function (action, subject) {
    if (action.id == "io.github.rthoms334.omatuner.update" &&
        subject.user == "<user>" && subject.local && subject.active) {
        return polkit.Result.YES;
    }
});
```
The policy has two actions on the same `exec.path`, told apart by `exec.argv1` (pkexec ≥ polkit
0.114): `…update` (`install`, auth_admin_keep) and `…update-passwordless` (auth_admin, always
the password). Other arguments match neither and fall to pkexec's generic action (the
password); the helper refuses them anyway (exit 2). The app's runner (`update::helper`):
no shell, stdin closed, output drained on threads (capped), 15 min for an install (the dialog
and pacman), 3 min for the switch; a stop or a time-out kills pkexec while it is still ours
(closing the dialog) and leaves a running helper alone. Exit codes → `helper::Outcome`.

### 41.6 Restart (`update::restart`)
* **Gate**: §41.4's, evaluated off the UI thread from the players' snapshots
  (`AppShared::tile_player`), the job manager and what the UI last said (`UiNow`: focus, an open
  overlay or text field or onboarding, multi-view, the screen); no frame yet counts as busy.
  "Unfocused" covers "not visible". **Casting** holds it too (`CastApi::activity`, §40): a
  session from `open` until its task let go of the TV (starting, casting, a stop still
  closing), Test on TV, a pairing (from `pair_start` until the PIN overlay closes, whatever the
  job's state) and the firewall's polkit dialog — the player alone misses these, as picture A
  is idle during Test on TV and pairing. The same activity holds the password dialog
  (`prompt_due`), and a pairing holds the passwordless install: it stays *Ready* and the
  restart watch installs it on its first tick after the overlay closed.
* **Exec**: `restart::request(Plan { view })`, then `PlatformEvent::Quit`: the window closes the
  normal way (`on_exit`: last screen, GL, `Services::shutdown`), `main` drops the instance lock
  and execs `/usr/bin/tuner [--profile-dir D] [--debug] [--view V]` — same pid, session,
  environment, cgroup; never the original command line (no second `--play` or playlist file).
  A window on a hidden workspace gets no frames (§3): like the signal watcher, after 3 s the
  updater runs the direct-exit cleanup (players stopped, writer flushed, socket removed) and
  execs from its own thread. An exec that fails (either way) is reported like a failed start,
  on the desktop too, and the process exits 1; a start that fails right after the restart
  names the new version and the way back (§21.7).
* **After**: the marker `$XDG_RUNTIME_DIR/tuner/restart.json` (`from`, `to`, pid, time, the
  Hyprland `Placement`: workspace and the window that had the keyboard) is taken by the new
  process when it is its own and ≤ 2 min old: toast "Tuner updated to X (from Y)", and under
  Hyprland `put_back` moves the new window to its old workspace without following
  (`hl.dsp.window.move{follow=false}`, legacy `movetoworkspacesilent`) and focuses the window
  that had the keyboard (`hl.dsp.focus`, legacy `focuswindow`). Workspace names and addresses
  are validated before they go into a dispatcher string.
* **Agents**: running `tuner mcp` bridges keep their old binary (pacman replaces the file; the
  old inode lives) and open one connection per request, so they carry on with the new Tuner;
  one launching Tuner meanwhile launches the new file (`ExeLauncher::current` strips
  " (deleted)"). An agent gets the new tools in a new session. The restarted Tuner is a normal
  start of a new version, so §39's start-of-version pass runs and, with *Keep Claude Code's
  plugin up to date*, updates the plugin.

### 41.7 Settings, toasts, notifications
Settings ▸ About & logs ▸ **Updates** (`settings/updates.rs`, a feature file of the About
page): the status line ("Tuner 0.2.5 · up to date · checked 12 min ago", "… is downloaded and
verified: it installs when you're back at Tuner with nothing playing", "Installing…", "… is
installed: it restarts when idle"), the last problem in red, *Check now*, *Install Tuner X now*
or *Restart now into Tuner X*, then for a pacman-managed Tuner the two switches —
*Automatically install updates* (`updates.auto_install`, on) and *Install updates without
asking for my password* (`updates.passwordless`, off; the screen's `adjust` sends it to
`set_passwordless`, the config follows the helper's success; "Waiting for your password…" or
the failure under it) — and for any other Tuner the sentence saying why it only tells
(`Why::text`). A note says what is installed and when it checks. `[updates]` is machine-local
(`Config::restored` keeps it). Toasts: *Install* (`Action::InstallUpdate`) on "Tuner X is
available" when it can install; *Restart now* (`Action::RestartToUpdate`) on "Tuner updated to
X"; neither action has a key. Desktop notifications: available (once per version, kept in
`update/told`), ready (once per version per session), installed.

### 41.8 Tests
No test reaches GitHub, runs pkexec, pacman or getent, or needs root. The tests' release key is
made from a seed in the test (`update/test_signer.rs`: aws-lc-rs Ed25519 + an RFC 7693
BLAKE2b, minisign's file format; a dev-dependency only). Unit: `vercmp` against pacman's table
both ways; stable tags; package file names; the committed key parses with id 7FC95FA61A0134E8
and the placeholder is no key; signatures (exact bytes, key, legacy, trusted comment); `pick`
(drafts, pre-releases, suffixes, numeric order, arch, unsigned); the API's JSON; pkexec / helper
exit codes; the runner's time-out and stop; the gate and its quiet period; the re-exec argv; the
marker; Hyprland placement parsing. The service against an axum GitHub with a fake pacman and
pkexec (shell scripts run by `/bin/sh`): download → private files → ready → install argv →
installed toast → 304 on the next check; passwordless installs at once; a dismissed dialog waits
for Install now; a tampered download is deleted and never reaches pkexec; a Tuner not from
pacman only tells, once; the rate limit; pacman newer than what runs; the switch follows the
helper; the gate reads the players. The helper (its own tests): a signed newer package goes to
pacman from root's copy (an interrupted run's leftovers gone first); `install` only hands the
path to the update service (its systemd-run argv, its exit codes passed on, "already loaded" is
busy, no control characters through); tampered, foreign, legacy and missing signatures; a signed
pre-release; the placeholder key; another name, arch, version not newer, not installed; busy and
failing pacman; relative, `..`, symlinked folder or file, hard link, open or writable folders,
over the cap; not root through pkexec, `install-as` through pkexec, bad arguments; the
passwordless rule's text, mode, removal and refused user names.
The real update (password, rule, restart, refusals) is `docs/MANUAL_CHECKLIST.md` §31.

### 41.9 Risks
* `exec.argv1` needs polkit ≥ 0.114 (Arch: 127). Older polkit would pick one of the two actions
  for both verbs.
* The restart's put-back is best effort: the new window maps on the active workspace and holds
  the keyboard for a moment before it is moved and the keyboard handed back.
* A window on a hidden workspace without `render_unfocused` stops drawing: its last published
  state (taken when it lost focus) decides the gate, and the restart exits without `on_exit`
  (no last-screen save; the screen comes from `--view`).
* Partial upgrades (§20.4) and `LocalFileSigLevel = Required` are pacman's; Settings shows
  pacman's message.
* The install runs as a transient system service (§20.4), so it needs systemd as PID 1 (Arch's
  `base`); without it `install` fails (exit 4, systemd-run's message). An install already in
  its service when Tuner quits or gives up waiting finishes; the next one meanwhile is busy.
* The download stops past 256 MiB whatever size GitHub lists for the package.

## 42. Watchlists

Design: `docs/design/watchlists-design.md`; its §11 records the approved decisions.

### 42.1 Lists and saved titles
Named, persistent lists of films, series, season ranges and episodes. In order follows the
first unfinished entry; Any order chooses an unwatched entry, preferring unfinished progress.
Done is derived across title copies, including watches outside a list. Start again changes
only the list's progress boundary and clears its skips. Placeholders keep missing titles in
place and attach when an import supplies their identity.

Alt+7 opens Watchlists; the sidebar entry is always present. Ctrl+F saves a film, series or
episode to My list; Ctrl+Shift+F opens Watchlists… with season ranges and insertion position.
Only My list puts a star on posters. Home's Your watchlists rail follows Continue watching;
detail pages show list membership. Favourites and their Settings page now hold channels only.
In the lists pane, Enter, → and a click select the list and open its entries; Play remains in
its menu, header and Home tile. A ticked list toggles off a single title or the exact series
range with Undo. Batch additions skip duplicates and report “N already there”. Save Up next
keeps pending film/series/episode rows and reports live or unavailable rows left out.

In the entries pane, Ctrl+W on a series or season range marks only its current episode:
the S/E shown by the row and the episode Play would start. The toast “Marked S01E04 watched”
offers Undo; the entry advances, becoming Done after its last unwatched episode. The menu
names this action “Mark S01E04 watched” (Ctrl+W) and offers “Mark all watched” for the whole
range (no key). A done series' “Mark unwatched” still clears the range; film and episode
toggles are unchanged. S/E numbers always show; episode titles obey the spoiler guard.
“Episodes…” / Ctrl+I opens series details with that episode's season selected and its row
focused, including for a range. Ctrl+I reuses List's Details key and opens film details too.
Esc / Alt+← returns to the same watchlist and entry with refreshed progress; a newly done
entry remains visible for that return even when other watched entries are folded.

### 42.2 Playing through Up next
`QueueCommand::PlayList` sets `QueueMachine.context` and adds one entry. When the queue's
pending rows run out, the engine derives the list's next entry afresh and counts down to it.
Rows added with Ctrl+N play first. Chips identify list entries; Then from … previews the list
without creating queue rows. The PIN, copies, resume, playback placement and media keys use
Up next's existing paths. A foreign play steps aside and retains the context; a restart never
auto-plays. Stop the list, just one, the sleep timer's end-of-queue and three failures end the
context; exhausting it shows That was … ✓. Ending the context discards only its automatic
pending rows, preserving Ctrl+N additions. Timed sleep stops and cancelling an unrelated
next-episode countdown retain the context. An ended movie or episode is excluded for the
rest of that run even if it did not reach watched_pct or has no duration; watched state is
unchanged. Series details load through jobs; an unavailable series is passed over and named
once for the run. A countdown's row is pre-resolved when it starts: reordering during it
changes the following pick. In order missing/agent-locked notices occur once per run; Any
order does not report entries merely preceding its random pick.

### 42.3 Storage and agents
Migration 0015 creates `watchlist` and `watchlist_item`, links queue rows to entries and moves
saved films and series out of favourites in one transaction. Only upgrades from v14 with
film/series favourites make one small `before-watchlists.json` user-data export, without
credentials or catalogue rows. Backup failure is logged and does not block the transaction.
Fresh installs and restores do not make that backup. Legacy restores merge same-name lists,
skip duplicate titles, and repair fallback identities when the catalogue attaches them.
The next launch explains the move once; OK, Open Watchlists or Esc acknowledges the card. Fingerprints, HOLDERS, import matching and backups
preserve lists across refreshes, re-keyed copies and playlist deletion.

The `watchlist` MCP tool reads and edits lists and starts or stops their playback;
`status.watchlist` identifies the active list. Locked entries are never played or added over
MCP, concealed entries are absent from all views and counts, and episode labels use the
existing spoiler guard. Plugin skills and RULES.md describe watchlists and their confirmations.
