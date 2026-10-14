# Casting design for Tuner (AirPlay to Apple TV first; Google Cast and DLNA later)

> Revision 2, 2026-10-12. This revision applies the user's decisions:
> - no hardware spike: the client is built on proven open-source implementations (§7.0);
> - native Rust AirPlay;
> - tvOS 26 and 27 from the start, with the Apple TV's auto-updates left on (§7.6);
> - an in-app "Allow on firewall" button (§9.4);
> - casting that respects workspaces (§3.8);
> - the doc's recommendation for every other question.
>
> §17 lists the decisions. The user checks the result on the real TV once, at the end (§18).
>
> Addendum, 2026-10-13 (after 0.3.0 in real use): "Cast to…" on every playable item and the
> **Play on** switch (§3.1, §3.9, the keys in §3.7), and the info panel while casting says what
> the TV is fed (§11.3). Once approved this becomes DESIGN **§40 Casting**, the next free number after §39. Code comments cite it as "DESIGN §40 §x.y". Appendix B lists what changed from revision 1.

This design is based on `main` at 17f9986. It was built from:
- four codebase maps: playback, UI, services and tests;
- five fact-checked research reports;
- three competing proposals: native-first, ship-fast sidecar and UX-first;
- four reference-research reports for revision 2: the pyatv video flow, fakes and fixtures, tvOS 26/27, and the firewall.

Appendix A explains how the proposals were judged and what each contributed.

Citations:
- **[AP]** AirPlay
- **[GC]** Google Cast
- **[DL]** DLNA and other receivers
- **[PL]** media pipeline
- **[NE]** this machine and the LAN
- **[REF]** the reference implementations, pinned in §7.0
- **[TV27]** the tvOS 26/27 research
- **[FAKE]** the fakes and fixtures research
- **[FW]** the firewall research

**Checked again in this pass:**
- **ffmpeg is always installed.** `mpv` depends on the `ffmpeg` package, which owns `/usr/bin/ffmpeg` and `/usr/bin/ffprobe` (9.0.2). The build has the `libx264` and `aac` encoders and the `-readrate` option. Every machine with Tuner therefore has the CLI.
- **Crates already compiled.** `Cargo.lock` already builds `aws-lc-rs` 1.18.1 (rustls' crypto backend), `axum` 0.8.9 (dev-dependency), `hyper` 1.12, `zbus` 5.19 (via mpris-server), `libc`, `socket2` and `tokio-rustls`. It does not have `mdns-sd`, `num-bigint` or `plist`.
- **mdns-sd version.** 0.21.5 came out on 2026-10-05, which is too new for the two-week rule. 0.21.4 (2026-09-21) is the version to pin. It adds `flume`, `if-addrs` and `socket-pktinfo`.
- **Backups.** `store/backup.rs` exports an explicit list of tables, so a new `cast_*` table never ends up in a backup.
- **Player internals.**
  - `Controller::run` is a crossbeam `select!` over commands, mpv events, the deferred END_FILE and a 250 ms ticker (`src/player/controller.rs:218`).
  - `state::Input` already has `HttpStatus` and `ProviderMessage`.
  - `PlayerErrorKind` is `Copy`.
- **Free names.**
  - `C` (Shift+c) is not bound in any key table.
  - `Integrations` holds `theme_watch, ipc, mpris, reminders, scheduler`.
- **Live preview autoplay would hijack a cast.** It plays whenever the player is Idle or showing its own preview (`live.rs:2212`, `guide.rs:566`), so it must be off while casting (§11.4).
- **Runtime directory.** `$XDG_RUNTIME_DIR` is a 1.6 GiB tmpfs here (15 GiB RAM).
- **Fixtures.** The Apple TV's and the Hisense's `/info` plists are at `/tmp/info_192.168.1.{5,112}.plist`, ready to become test fixtures once scrubbed (§5.3).
- **Reference sources** were read at pinned commits (§7.0). The local read-only clones are in `/tmp/pyatv-research/` and the PR diffs in `/tmp/ap_research/`.
- **Interlacing is settled by Apple's spec.** HLS Authoring Specification rule 1.14 says "All interlaced source content MUST be deinterlaced." This answers what Phase 0 was going to measure (§8.3).
- **Firewall state can be read without root** [FW]:
  - `/etc/ufw/{ufw.conf,user.rules}` and `/etc/default/ufw` are 0644;
  - `systemctl is-active ufw` works without root;
  - `ufw status`, `nft` and `iptables` do not.
- **The polkit agent is Omarchy's shell.** Quickshell's `PolkitAgent` (`/usr/share/omarchy/shell/plugins/polkit/`) draws a themed password or fingerprint dialog. The user `r` is in `wheel`, which polkit treats as admin.
- **Whether Tuner's window is on screen** is already answered by `platform::popout::self_shown()` (Hyprland, DESIGN §30.13). It reports whether the window is on a workspace that a monitor shows.

---

## 0. Decisions made here (not open questions)

1. **The TV never gets a provider URL.** Tuner always relays. It fetches the stream itself, with the provider's headers and under its connection budget, and serves the TV a LAN URL that holds only a one-session token.
   - No protocol gets a "direct URL" option, not even for M3U URLs without credentials.
   - Xtream URLs carry the login in the path (DESIGN §20).
   - Receivers can't send the `User-Agent`, `Referer` or `Cookie` headers some providers need (`merge_headers`, `resolve.rs:316`).
   - A connection opened by the TV would be invisible to the connection budget.
2. **Casting is an output of picture A's controller, not a new player.**
   - The controller keeps doing everything it does today: resolving, the PIN, explicit and shield gates, zapping, the reconnect policy, copy fallback, progress, history, the queue's `Ended` and the budget lease.
   - Only where the picture goes changes.
   - So the OSD, keys, MPRIS media keys, the sleep timer, Up next and the agent's `control` all reach the TV without being changed.
3. **AirPlay comes first. It is written in native Rust and ported from proven implementations. It uses only the `/command` play-queue flow.**
   - Every receiver on the user's LAN is AirPlay ([GC A.2], [NE A]).
   - Legacy `POST /play` is broken on tvOS 26.x, including 26.6 on this Apple TV (pyatv #2906), and on 27.0 (rust-atv) [TV27]. It is never implemented.
   - Each protocol step is ported from a named upstream source that has hardware evidence (§7.0). Where the sources disagree, §7 picks one and cites the reason.
4. **ffmpeg makes the HLS, and Tuner feeds it.**
   - The `ffmpeg` CLI runs as a supervised child process. It reads a loopback URL served by Tuner's upstream proxy, so it never sees a provider URL or login.
   - For AirPlay the output is always HLS:
     - a live MP4 pipe failed (castcli #35);
     - AVPlayer won't play progressive TS or MKV [PL], [NE C.2].
5. **Casting moves the stream.** Local mpv stops, and the relay's single upstream connection becomes picture A's lease. There is no "play on both".
6. **Multi-view, pop-out and casting exclude each other in v1.**
7. **Tuner never runs `sudo` and never sees a password.**
   - If ufw blocks the TV, the **Allow on firewall** button runs one narrowly scoped, validated rule change through `pkexec`. polkit's graphical agent asks for the password (§9.4).
   - A command to copy is always shown as the fallback.
   - The 10 s "did the TV fetch" check is the end-to-end truth.
8. **No hardware spike, and no blind build.** Casting to a TV is a solved problem. The client follows the open-source senders that work on tvOS 26.5, 26.6 and 27 [REF].
   - The test fake is built from those projects' own fakes, tests, captures and published protocol notes (§14.3), not from a session on the user's TV.
   - The user checks the result on the real TV at the end (§18).
9. **tvOS 26 and 27 share one code path.** Every tvOS 27 difference is handled by behaviour that is also correct on 26, so nothing branches on the version. The version is read only for logs and `diagnose` (§7.6). The Apple TV's automatic updates stay on.
10. **Casting never moves Tuner's window.** Alerts follow the user to whatever workspace they are on (§3.8).

---

## 1. Goals and non-goals

**Goals**
- Send whatever is playing to "Living Room" from the player with one key (`C`) or click, and bring it back with "Play here". That covers live TV, catch-up, films and episodes.
- Keep every Tuner guarantee:
  - no stream address or login leaves the process;
  - parental and spoiler rules still apply;
  - one-connection accounts still get exactly one connection.
- Everything that controls playback today also controls the TV: the OSD, keys, MPRIS media keys, the sleep timer, Up next and Claude.
- Work on tvOS 26 and 27. The Apple TV keeps auto-updating.
- Set up inside the app (user rule: surface opt-in setup in the app):
  - a first-run card when a TV is found;
  - Settings ▸ Cast;
  - PIN pairing on Tuner's own screen;
  - the firewall fix as one button in the app, with the command as fallback.
- Behave well when Tuner is on another workspace (§3.8).
- Google Cast and DLNA should each be a new `Receiver` implementation, not a new pipeline.

**Non-goals for v1**
- Screen mirroring, AirPlay audio (RAOP), legacy AirPlay `/play`.
- Playing on the computer and the TV at the same time; casting in multi-view; casting both pictures.
- Subtitles on the TV and choosing the TV's audio track (Phase 2). Surround sound is in Phase 1 (§8.4).
- TV volume over AirPlay. No verified way to do it exists [AP 2.5]. pyatv PR 2919 does volume over a control-only session, but only with transient pairing and only tested on speakers [REF].
- Receivers other than AirPlay in Phase 1:
  - Google Cast is Phase 2 and DLNA is Phase 3.
  - Roku, Kodi JSON-RPC and FCast are optional, Phase 4.
  - LG SSAP, Tizen native, Fire TV, Sonos and Miracast are never planned [DL §0].
- Macs as receivers. pyatv marks `Mac*` models as Unsupported [NE A.3], and a Mac answers `/pair-pin-start` with 403 (castcli).
- Low latency. Live TV on the TV runs about 10–20 s behind because of HLS hold-back [PL §1], and the UI says so.

---

## 2. User stories

1. **Move the match to the TV.** I'm watching Sky Sports at the desk. I press `C`, pick Living Room and press Enter. The laptop shows "Playing on Living Room · Live · about 15 s behind" and the TV shows the match. Space pauses the TV, and ↑ goes to the next channel on the TV.
2. **First time.** The first time I pick Living Room, Tuner says "Enter the code shown on Living Room". I type the 4 digits from the TV into Tuner. It pairs and starts.
3. **Firewall.** Nothing happens on the TV. Tuner says "Living Room couldn't connect back to Tuner. Your firewall is blocking it." and offers **Allow on firewall**. I click it and Omarchy asks for my password. Tuner then plays its test card on the TV.
   - **Show command** gives me the `sudo ufw …` line instead, if I'd rather run it myself.
   - **Remove rule** in Settings ▸ Cast takes the rule away again.
4. **A film from where I left off.** I cast a film at 1:10:00 and the TV starts at 1:10:00. When it ends, the progress is saved as watched, and Up next plays the next episode on the TV.
5. **Back to the laptop.** "Play here" stops the TV. The laptop carries on from the TV's position for films and catch-up, or at the live point for live TV.
6. **Bedtime.** The sleep timer stops the TV at 23:00.
7. **Claude.** "Put the news on the Apple TV" becomes `guide_search`, then `play {ref, device}`.
   - Tuner is on another workspace, and nothing there moves.
   - A locked channel brings Tuner forward for the PIN.
   - An unpaired TV gets "Pair Living Room in Tuner first (Settings ▸ Cast)".
   - If the TV can't connect, a desktop notification says so. Its **Show** button brings Tuner forward on the fix.
8. **One-stream account.** "TV" allows one stream. Casting doesn't open a second one: the laptop stops and the TV takes over.
9. **A channel the TV can't show as-is.** A 576i channel with MP2 sound plays, because Tuner deinterlaces the picture and converts the sound. If a progressive channel's picture looks wrong, **Convert this channel** in the player menu re-encodes it, and Tuner remembers this for that channel and that TV.
10. **The TV updated overnight.** The Apple TV is now on tvOS 27 and casting still works. If the update reset "Allow Access", Tuner says which setting to change.

---

## 3. UX

### 3.1 Choosing the TV

**Entry points:**
- the OSD **Cast** control (wide layout, before Fullscreen);
- the player menu item **Cast to…**;
- the key `C` in the player;
- the agent;
- **"Cast to…" on any playable item** (0.3.0 field feedback: "no way to just click a button to
  cast without first starting it in the player"): the item menu of a Live TV channel row, a
  Home tile, a film or series card, a title's page, an episode row and a guide programme, and
  the key `Action::CastItem` on the focused one (§3.7). Code: `ui/casting.rs`, hooked into
  `ui::item_menu::extend` / `dispatch` like the batch features.
  - Exactly one TV ready (usable, paired or needing no pairing; a remembered TV not seen this
    run counts: its stored address is tried, and a TV that's off gets the typed error card):
    the entry reads "Cast to Living Room" and casts straight there, with the toast "Casting BBC
    One to Living Room". Several, or none ready: "Cast to…" opens this panel for that item
    (`OverlayRequest::CastItem`): headed `CAST “BBC One” TO`, no "This computer" row; an
    unpaired TV pairs first, then casts the item (`AfterPair::Item`).
  - The item plays exactly as Shift+Enter plays it here — the same PIN prompt
    (`Action::CastUnlocked` comes back to the app, which casts it then), the same copy with its
    fallback, the same Resume / Start over prompt for a film or an episode — only its commands
    go to the TV (`UiCtx::cast_with`: `Play` → `CastTo`, `PlayCopy` → `CastCopy`, a Resume
    prompt opened meanwhile casts its answer). Nothing plays here first: the TV's first load is
    the relay's, under picture A's lease (§10). The screen stays; the mini player shows the
    casting card.
  - A programme casts its channel while it airs and its catch-up once it aired (on a channel
    with an archive); one that hasn't started isn't offered (the reminder entries are).
  - Not in multi-view (§0.6): the entry isn't offered there, and the key says "Leave
    multi-view to cast".
- **Play on** (§3.9): a switch that sends every play to a TV.

```
                                         ┌ Cast to ─────────────────────────┐
                                         │ ▸ This computer               ✓  │
                                         │                                  │
                                         │   Living Room                    │
                                         │   Apple TV 4K (3rd gen…) · Paired│
                                         │   Living Room TV                 │
                                         │   Hisense 85E75LE · Pair first   │
                                         │   OT Macbook Air                 │  ← greyed, not selectable
                                         │   Mac · Can't cast to Macs       │
                                         │ ━━━━━━━━━━━━━━  Searching…       │
                                         │   Add by address…                │
                                         │   Firewall help…                 │
                                         └──────────────────────────────────┘
```

**Layout and rows**
- It is a right-edge panel in OSD colours, so it is readable over video. It is built like `ui/overlays/tracks.rs` (`Row`, `cursor`, `place_cursor`, `choose`).
- "This computer" comes first, then the devices, most recently used first.
- The second line of each device row says what the TV is, then its state (`Device::make`):
  - a third-party TV by its TXT/`/info` `manufacturer` and `model`: "Hisense 85E75LE" ("AirPlay TV" alone read as a generic entry next to the Apple TV). The maker is kept in memory from this run's discovery (the store has no column; the picker browses while open), so a remembered TV not announced yet shows "AirPlay TV 85E75LE";
  - an Apple TV by its model identifier's name where Tuner knows it ("AppleTV14,1" → "Apple TV 4K (3rd generation)"), else "Apple TV", never a guess;
  - both strings are the device's own: sanitized and capped like its name (§6 "Untrusted strings").

  | Row text | Meaning |
  |---|---|
  | `Apple TV 4K (3rd generation) · Paired` | Ready to cast. |
  | `Pair first` | Needs one-time PIN pairing. |
  | `Mac · Can't cast to Macs` | Greyed out. |
  | `Same Home only` | Greyed out, with the setting hint. |

- No IP addresses appear anywhere in the panel.

**Discovery feedback**
- "Searching…" with `widgets::progress::busy_bar` while discovery runs.
- After 6 s with nothing found: "No TVs found" plus "Firewall help…".
- "Add by address…" opens a one-line IPv4 entry, which probes `GET /info` on port 7000.

**Choosing a row**

| Row | Enter does |
|---|---|
| A paired device | Sends `PlayerCommand::CastTo`. |
| An unpaired device | Opens the pairing overlay (§3.2), then sends `CastTo`. |
| "This computer", while casting | **Play here**. |

In multi-view the panel shows a single line, "Leave multi-view to cast".

### 3.2 Pairing

```
┌ Pair with Living Room ─────────────────────────────────┐
│ Enter the code shown on the TV.                        │
│                                                        │
│              ┌───┐  ┌───┐  ┌───┐  ┌───┐                 │
│              │ 4 │  │ 7 │  │ _ │  │   │                 │
│              └───┘  └───┘  └───┘  └───┘                 │
│                                                        │
│ No code on the TV? On the Apple TV open Settings ▸     │
│ AirPlay and HomeKit ▸ Allow Access ▸ Anyone on the     │
│ Same Network.                     [ Show a new code ]  │
│                                     [ Cancel ] [ Pair ] │
└────────────────────────────────────────────────────────┘
```

**Input**
- Has the 600 ms `ARMING` guard from `agent_connect.rs`.
- Digit keys and Backspace edit the code. Enter pairs once 4 digits are in.
- **Show a new code** asks the TV for a fresh PIN using the next pairing variant (§7.2).

**PIN handling**
- The PIN goes from the overlay to the cast service once, then it is dropped. It is never logged, stored or shown in `Debug` output.
- It is deliberately **not** passed to `register_secret`: a 4-digit secret would mask every 4-digit number in the logs.
- The overlay keeps the pairing connection open while the user types. The Apple TV accepts the PIN only on the connection that asked for it (castcli) [REF].

**Replies and their copy**

| Reply | Copy |
|---|---|
| Wrong code (Error TLV 2) | "That code didn't match. The TV is showing a new one." |
| Too many tries (Error TLV 3, BackOff) | "Living Room wants a short break after several tries. Try again in a minute." |
| HTTP 470 on both variants, or no code appeared | "On the Apple TV: Settings ▸ AirPlay and HomeKit ▸ Allow Access ▸ Anyone on the Same Network, and give it a room in the Home app." Apple resets Allow Access on major upgrades [AP 1.4]. |
| The code expired (the connection closed while waiting) | "The code expired." + **Show a new code** |
| Timeout | "Living Room isn't answering. Is it on?" |

On success the toast "Paired with Living Room" appears and the cast starts.

### 3.3 While casting

```
┌────────────────────────────────────────────────────────────────────────────────────┐
│ 󰁍  401 Sky Sports Main Event   ● TV                       󰄙 Living Room   20:26 │
│                                                                                    │
│                                  ┌──────────────┐                                  │
│                                  │    [logo]    │                                  │
│                                  └──────────────┘                                  │
│                              Sky Sports Main Event                                 │
│                     Semi-final 1 · 19:45–22:00 ▕██████░░░░▏ 1h34                    │
│                                                                                    │
│                           󰄙  Playing on Living Room                                │
│                              Live · about 15 s behind                              │
│                                                                                    │
│                       [ Stop casting ]     [ Play here ]                           │
│                                                                                    │
│ 󰒮 󰐊 󰒭   󰐑 Channels  󰃰 Guide  󰋚 History  󰋽 Info  󰄙 Living Room  󰊓 Fullscreen  │ 󰕾 TV remote │
└────────────────────────────────────────────────────────────────────────────────────┘
```

The glyphs are nf-md `cast` / `cast_connected`. Check their codepoints against the bundled JetBrains Mono Nerd Font, which covers U+F0001–F1AF0.

**The casting card**
- It replaces the video area. It is an early branch in `VideoHost::paint_state_overlay` that reuses the audio-only layout (`video_host.rs:~476-500`).
- It shows the logo, the title and now/next (through the same spoiler-guard helper as the OSD), "Playing on Living Room", and a phase line:

  | Phase line | When |
  |---|---|
  | Connecting to Living Room… | The session is starting. |
  | Starting on Living Room… | Before the first picture. |
  | Tuning Living Room to BBC One… | While zapping. |
  | Live · about 15 s behind | Playing live TV. |
  | Paused | — |
  | Buffering on the TV | — |
  | Seeking… | — |
  | Converting picture | L3 is active (§8.3). |

- Buttons: **Stop casting** and **Play here**. Error states add their own action (§3.4).
- The player menu gets **Stop casting**, **Play here** and **Convert this channel** (L3 for this item on this TV, remembered). When the TV is fed L3 only because it was forced (that choice, or an L2 failure remembered for the item on this TV, §8.3), the last item is **Send as is** instead: it forgets both and loads the item again as the plan alone decides, where the TV is, with the escalation step back.

**The OSD while casting**
- The controls row stays. Controls that do nothing on the TV are hidden: audio track, subtitles, aspect and zoom.
- The info panel shows the probe result (§8.2) instead of mpv's stream info.
- The speaker reads "TV remote", with the tooltip "Use the TV remote for volume" (AirPlay).
- The Cast control shows the connected glyph, with the device name as its live text (`control_text`).

**Other surfaces**
- **Compact layout:** a status chip "Casting · Living Room", whose ✕ means Stop casting.
- **Mini player:** the same card, plus a cast glyph in its strip.
- **Pop-out:** disabled, with the message "Nothing to show here — playing on Living Room". A cast that starts while the window is popped out puts it back in, unraised (§3.8).
- **Everything played while casting goes to the TV.** That includes Enter in Live, Guide or Home, zaps, Up next, reminders' switches and Claude, because all of these play in picture A.
- **Live preview autoplay is off.**
- **Quitting** while casting asks first: "Closing Tuner stops playback on Living Room". Tuner *is* the relay, so the TV can't carry on without it.

### 3.4 Errors

Each error is one sentence plus an action. Where it appears depends on what the user can see (§3.8):
- in the casting card when the player is on screen;
- as a toast (`Toast::error`) when the window is seen but the player isn't;
- as a desktop notification with **Show** when the window isn't on a visible workspace.

The copy never contains device text or provider text.

| Kind (`CastErrorKind`) | Copy | Actions |
|---|---|---|
| `Firewall` | "Living Room couldn't connect back to Tuner. Your firewall is blocking it." | **Allow on firewall** · Show command · Try again · Play here |
| `Unsupported` | "Living Room can't play this channel's format." When relevant, adds "Converting is off in Settings ▸ Cast." | Play here |
| `CpuTooSlow` | "This computer can't convert this channel fast enough." | Play here |
| `NeedsFfmpeg` | "Casting needs ffmpeg (it's normally installed with mpv)." | — |
| `Unreachable` | "Living Room isn't answering. Is it on and on the same network?" | Try again · Play here |
| `PairingRequired` / `PairingLost` | "Living Room needs pairing." / "Living Room forgot Tuner. This can happen after a TV update." | Pair |
| `PairingRefused` | The 470 hint from §3.2. When pair-verify got an empty reply: "On the Apple TV set Allow Access to Anyone on the Same Network. 'Everyone in your Home' blocks Tuner." | Pair |
| `StoppedOnTv` | "Living Room stopped playing. Its remote stopped it, or another device took over." | Play here · Cast again |
| `ReceiverFailed` | "Living Room couldn't play it." Tuner first escalates one level itself (§8.3). | Convert this channel · Play here |
| Upstream errors | Unchanged: "Reconnecting… attempt n", the `Budget` copy, provider messages. The relay sees real HTTP statuses, so the reconnect policy works as is. | as today |

### 3.5 First-run card and setup

**The discovery pass**
- `start_cast_pass` is modelled on `start_agent_pass` (`app.rs:317`).
- It runs one 5 s mDNS browse at startup, at most once per Tuner version, and only when `cast.offer_setup` is on and `off_desktop()` is false.

**When the card appears**
- Only when the pass found a usable, video-capable, **unpaired** AirPlay receiver: an Apple TV or a third-party AirPlay TV (§7.7), the first in list order.

```
┌──────────────────────────────────────────────────────────────────┐
│ 󰄙  Watch on your TV                                              │
│ Tuner found Living Room (Apple TV) on your network. Pair it once │
│ with the code the TV shows, then press C in the player to send   │
│ what you're watching there.                                      │
│                                                                  │
│ [ Set up Living Room ]   [ Not now ]   [ Don't ask again ]       │
└──────────────────────────────────────────────────────────────────┘
```

**The gate**
- It reuses `CardGate` / `agent_card_may_show` (`app.rs:2762-2780`). The card only shows when:
  - onboarding is done;
  - no overlay is open;
  - the user is not in the player, fullscreen, the pop-out or multi-view;
  - the user is not typing;
  - the user is not on Settings ▸ Cast.
- **Only one card at a time.** If the agent card is due, it goes first, and the cast card waits for the next frame where the gate passes.

**The setup wizard**

"Set up" runs a three-step wizard in the card's place:
1. **Pair** (§3.2).
2. **Network.** Shown only when detection says ufw blocks the port. It offers **Allow on firewall**, **Show command** and **Check again** (§9.4). The button shows "Waiting for your password…" with Cancel while polkit's dialog is up.
3. **Test on TV.** A 15 s Tuner test card goes through the real relay and pipeline. It needs no provider connection, and it proves pairing, the firewall path and the format in one go.

It ends with "Ready. Press C in the player to cast."

**Answers**
- Every answer writes `cast.checked_version`.
- "Don't ask again" also writes `cast.offer_setup = false`. That is the same switch as Settings ▸ Cast ▸ "Offer to set up new TVs".

### 3.6 Settings ▸ Cast

The page sits after AI agents. Its template is `settings/agents.rs`, whose header doc is the page wireframe.

```
Settings ▸ Cast
 Casting
   Cast to TVs                                         [on]
   Offer to set up new TVs                             [on]
   Play on: This computer…                             (the chooser, §3.9)
   Remember Play on across restarts                    [off]
 TVs
   Living Room       Apple TV 4K (3rd generation) · Paired · tvOS 26.6 · used today ⋯   (Pair again · Forget · Make default)
   Living Room TV    Hisense 85E75LE · Not paired                                   ⋯   (Pair · Forget)
   Search again
   Add by address…
 Network
   Tuner serves the TV from this computer (wlan0), port 47800.
   Firewall              Blocked (ufw)   [ Allow on firewall ] [ Show command ] [ Check again ]
   Firewall              Allowed (Tuner's rule)               [ Remove rule ]  [ Check again ]   ← once allowed
   Port                                                47800
   Test on TV…
 Picture and sound
   Convert what the TV can't play       Automatically (uses the CPU) │ Never
   Interlaced channels                  Deinterlace (Apple's rule) │ Send as is
   Keep this computer awake while casting   [on]
 Live TV on the TV runs about 10–20 s behind the computer.
```

- The rows reuse `Row::Section`, `Row::Toggle`, `Row::Choice`, `Row::Item` + menu and `Row::Act`.
- The device list is read from `CastHandle::devices()` every frame, with `request_repaint` while searching.
- The version in a TV row comes from its TXT `osvers` or `/info` (§7.6). It is a sanitized device string, shown only on this page.
- The firewall row shows the detected state (§9.4):
  - **Allow on firewall** appears only for ufw with `pkexec` available;
  - **Remove rule** appears only when a rule tagged `omatuner cast` exists;
  - a rule for another network reads "Allowed on another network (eth0)", with Remove.
- "Sound on the TV" is not in Phase 1: it is AAC stereo (§17).

### 3.7 Keyboard

| Key | Action | Where |
|---|---|---|
| `C` | `Action::CastMenu`: "Cast to a TV…". While casting it opens the picker with "This computer" focused, so `C` then Enter is Play here (back to this computer). | `PLAYER_COMMON` (checked free) |
| `C` | `Action::CastItem`: "Cast to a TV…" on the focused item (§3.1). | `GUIDE`, `DETAIL` (no filter there; checked free) |
| Alt+C | `Action::CastItem` | `ITEM_ACTIONS` (lists, grids, Home, detail pages, filtering or not), `SEARCH_ITEM_ACTIONS`, `GUIDE`. Lists and grids filter on every printable key, so a plain `C` there would steal a filter's capital C — and with one paired TV, typing "CNN" would cast. |
| Alt+T | `Action::PlayOnMenu`: "Play on… (this computer or a TV)", the chooser (§3.9). T for TV; Alt+C is the item's. | `GLOBAL` (works from a text field too; Omarchy binds no Alt+letter) |
| — | `Action::StopCasting`: "Stop casting". No default key; it is in the menu, the card and the chip (the picker offers Play here, `StopCasting { play_here: true }`). | — |
| — | `Action::PlayHere`: "Play here" on the alert after the TV stopped the cast (§3.4 "Play here · Cast again"). No default key. | — |
| Enter / ↑↓ / Esc | Picker and PIN overlay. They reuse the `Modal`/`List` contexts, so there is no new `KeyContext`. | — |

- There is no global chord, because Ctrl+Shift+C is the terminal copy chord.
- `feature_on`: `A::CastMenu | A::StopCasting | A::PlayHere => crate::cast::ENABLED`. With casting switched off in Settings (`cast::on`), `C` and the player's cast controls do nothing and aren't shown; a cast that still runs keeps its way off.
- Regenerate DESIGN §15.2 and the README key tables with `cargo run --example keymap_md`.

### 3.8 When Tuner isn't on a visible workspace

Casting is the one kind of playback that doesn't need Tuner's window: the picture is on the TV. So Tuner is often on another workspace, or behind the agent's terminal, while it casts. Two rules apply: casting never moves the window, and anything that needs the user reaches them where they are.

**Is the window seen?** This is decided the way an agent's play decides it (DESIGN §30.13).
- The window is *seen* when it is focused, or when `platform::popout::self_shown()` says it is on a workspace a monitor shows. That means pinned, or on a monitor's active or open special workspace, and not a hidden group tab.
- That call blocks on two Hyprland socket requests, so it runs on the runtime (`spawn_blocking`) and only when an alert is due.
- Off Hyprland, or when Hyprland doesn't answer, an unfocused window counts as **not seen** for alerts.
  - This is the opposite default to §30.13, which never moves a window it isn't sure about.
  - Alerts only tell, they never move anything. A duplicate notification is harmless; a missed firewall error is not.

**Casting never moves the window**
- **An agent's `play {device}`** stays in place: no pop-out, no raise, no workspace change, whatever "Agents play in" says. The result says `opened_in: "device"` (§12).
- **A reminder's automatic switch while casting** retunes the TV and skips its "Open in" placement.
- **The one exception** is the same as for every agent play: an item that needs the parental PIN raises the window (`hypr::focus_self`) with the PIN prompt, because the PIN is typed there (§30.13 step 2). Nothing is cast before the unlock.
- **A cast that starts while the window is popped out** (from any source) puts it back into the window, **unraised**. This reuses the `AgentCommand::PopIn` path. A pop-out pinned on every workspace would only show the casting card.

**Alerts reach the user.** Each cast event that needs the user goes to the first matching place:

| Window | Player screen | Where the alert goes |
|---|---|---|
| seen | on screen | the casting card and its buttons |
| seen | not on screen | a toast with the action (§3.4) |
| not seen | — | a desktop notification (`platform::notify`, the same fixed sentence, no device or provider text) with **Show**. Show raises Tuner (`hypr::focus_self`) onto the player, where the casting card has the buttons, such as Allow on firewall. Without a notification server, the toast, as today. |

- **What alerts.** Every `CastErrorKind` (§3.4), including `Firewall` from the 10 s fetch check and `StoppedOnTv`. Phase changes (connecting, playing, paused) never alert.
  - The common case: the user starts a cast in Tuner, switches workspace, and the TV fails 10 s later.
- **Up next waiting for the PIN while casting** sends its "Up next is waiting for your PIN" notification when the window isn't focused.
  - Today's rule keys on a suspended local video. That never happens while casting, because there is no local video.
- **Quit while out of sight.** A close request from the compositor, such as a close-window bind used on another monitor, raises the window first. That way the confirmation "Closing Tuner stops playback on Living Room" is seen.
- **The agent's view.** `status.casting` carries `phase` and, after a failure, a typed `error`. An agent whose user is in a terminal can then say what happened without Tuner being visible (§12).
- **Pairing and the firewall prompt** start from a click in Tuner, so the window is seen at that moment.
  - The PIN overlay waits if the user leaves its workspace. If the TV drops the code meanwhile, the overlay says "The code expired" (§3.2).
  - The polkit dialog is drawn by Omarchy's shell on the focused monitor, not inside Tuner's window, so it appears where the user is.
- **Power.** The logind sleep inhibitor doesn't depend on the window (§11.5).
  - Hyprland keeps rendering Tuner on hidden workspaces (`render_unfocused`, DESIGN §22). Casting needs no local rendering at all.

### 3.9 Play on

```
                                         ┌ PLAY ON ─────────────────────────┐
                                         │ ✓ This computer                  │
                                         │   Living Room                    │
                                         │   Apple TV 4K (3rd gen…) · Paired│
                                         │   Living Room TV                 │
                                         │   Hisense 85E75LE · Pair first   │
                                         │   Add by address…                │
                                         │   Firewall help…                 │
                                         └──────────────────────────────────┘
 status bar:  … Ctrl+F fav · Shift+F10 more      󰄙 Play on: This computer 󰅀   Guide updated 10 min ago
 sidebar:      ┌──────────────────┐   (a TV is the target: 󰄙 in the accent colour, "Living Room")
               │ 󰄙 This computer  │   rail: 󰄙 alone, tooltip "Play on: This computer (Alt+T)"
               │ 󰍉 Search         │
               │ 󰋜 Home …         │
```

**What it is.** A switch for where plays go: this computer, or a TV. With a TV chosen, every
play of picture A goes there instead of mpv — Enter or a click on a channel, a film, an
episode, the guide's Watch, a Home tile, Up next, a reminder's switch, `--play`, and an agent's
`play` without `device` — and the player screen shows the casting card where it would open.

**Where it lives.** Picture A's controller holds it (`PlayerCommand::SetPlayOn`,
`PlayerSnapshot::play_on`): a `Play` or `PlayCopy` while the output is local becomes
`cast_to(device, play)` — so every sender follows it without knowing (the screens, the app, Up
next's engine, the agent) and the PIN, copy and Resume rules stay the senders' as for a local
play. Not diverted: the Live preview's plays (`PlayOrigin::LivePreview`: moving the cursor never
casts; the preview autoplay is off anyway while the switch is on a TV, `live.rs` / `guide.rs`),
and anything in multi-view (two pictures are this computer's; the switch applies once it ends).
Multi-view that ended on picture B: a play while the switch names a TV, and a *Cast to…*, go to
picture A as a film does (picture B stops, its claim goes at once: `app::mv_vod_to_a`).
On a TV already, plays land on that TV as before (§3.3).

**The sidebar control.** The most used of the entry points, so it heads the left sidebar, directly
above Search: `widgets::sidebar::PlayOn` — a cast glyph and "This computer" / the TV's name (the
TV in the accent colour, with the connected glyph, so it is plain that plays go there); the rail
and the Compact glyph strip show the glyph alone, its tooltip "Play on: Living Room (Alt+T)". A
click or Enter opens the same chooser as the chip (`Action::PlayOnMenu`). It is a control, not a
screen: never the lit tab, no `ScreenId`, no Alt+digit. It shows on the chip's condition
(`App::play_on_target`: casting on and a TV ready, or a cast or the switch names one), so a
sidebar without TVs is unchanged; unlike the chip it also shows beside the Compact cast chip.
The video-only and pop-out windows, Onboarding and the player have no sidebar, so none either.

**The chooser** (`OverlayRequest::PlayOn`; the sidebar control, the status chip, Alt+T, Settings ▸ Cast ▸ *Play on:
…*): this panel headed `PLAY ON`, the mark on where plays go now (the TV casting, else the
switch's). A choice works like a speaker picker (`ui::casting::choose`): what plays here moves
to the TV chosen (`CastTo { play: None }`; a Live preview stops instead), a cast moves to the TV
chosen, and "This computer" while casting is Play here. An unpaired TV pairs first
(`AfterPair::PlayOn`). The chip ("󰄙 Play on: Living Room 󰅀") shows while casting is on and a TV
is ready, or a cast or the switch names one; not beside the Compact cast chip, which says it.

**It only changes when the user changes it.** A TV that doesn't answer shows the typed error
card (§3.4) with Play here, and the switch stays; Stop casting and Play here leave it too (Play
here plays that item here; the next play goes to the TV again). A session that can't even open
(the TV forgotten, casting switched off meanwhile) toasts the failure with **Play here**, which
plays the item that was on its way (the controller keeps it: `undelivered`). Forgetting the
TV in Settings and switching casting off put the switch back on this computer.

**Across restarts.** The switch lasts for the session. It is written to `cast.play_on` whenever
it changes and restored at start only with Settings ▸ Cast ▸ *Remember Play on across restarts*
(`cast.remember_play_on`, off): by default every start plays here, so a TV that's off at the
next start never swallows plays.

**Agents.** `status.play_on` names the switch's TV (single view), and a `play` without `device` while it names
one is answered as a cast (`opened_in: "device"`, the TV named, the window left in place, §3.8),
as a play while casting is. Agents never change the switch: it is the user's (RULES.md §2, §12).
Reminder switches follow it too, and skip their "Open in" placement as while casting.

---

## 4. Receiver support

### 4.1 Receivers

| Receiver | Protocol | Phase | On the user's LAN today |
|---|---|---|---|
| Apple TV 4K (tvOS 26.x and 27) | AirPlay 2 video, `/command` play queue | **1** | "Living Room": AppleTV14,1 (4K gen 3), tvOS 26.6, 192.168.1.5:7000.<br>- `/info` statusFlags 0x118644, TXT flags 0x18644; bit 9 means pairing is mandatory.<br>- Features bits 0, 4, 33, 38, 48 and 49 [NE A.1].<br>- Auto-updates are on; both versions are supported (§7.6). |
| Third-party AirPlay TVs (Hisense VIDAA, Samsung, LG) | same, third-party profile (§7.7) | 1 | "Living Room TV": Hisense 85E75LE.<br>- Sets bits 49 and 33 but not bit 0 (so no legacy `/play`); flags 0x244.<br>- Its player is hls.js, so expect HLS only [NE A.2].<br>- VioletRelay reports a Hisense 100E75LE from the same family working [REF]. |
| Mac AirPlay receiver | — | never | "OT Macbook Air" (`act=2`) |
| Chromecast, Google TV, TVs with Cast built in (Sony, TCL, Hisense, LG 2024+, Samsung 2026) | CASTV2 Default Media Receiver `CC1AD845` | 2 | none found |
| DLNA/UPnP MediaRenderer (older Samsung/LG, Panasonic, Philips, Kodi, AV receivers) | AVTransport + RenderingControl | 3 | None seen. This is inconclusive, because ufw drops unicast M-SEARCH replies [NE]. |
| Roku (Play on Roku `15985`), Kodi JSON-RPC, FCast | — | 4, optional | — |

### 4.2 Content × receiver → output path

The paths are defined in §8.3:
- **L1:** byte relay.
- **L2:** remux to HLS; video copied, audio copied or converted to AAC stereo.
- **L3:** video re-encoded with libx264 (plus `yadif` for interlaced input), then HLS.
- **R:** refused.

| Content (from hints or probe, §8.2) | Apple TV (P1) | Third-party AirPlay (P1, exp.) | Chromecast (P2) | DLNA (P3) |
|---|---|---|---|---|
| Live TS or HLS: progressive H.264 ≤ High@5.2 (Apple 1.3b), AAC ≤ 2 ch | L2, TS segments, copy and copy | L2 | L2 + CORS. L3 when above the model's level (gen 1/2: ≤ 4.1, 1080p30) | L1 `video/mp2t` |
| Same, audio MP2, AAC-LATM, AC-3/E-AC-3 or AAC 5.1 | L2, audio converted to AAC stereo (Apple rule 2.3 requires stereo AAC); with more than two channels, plus a surround rendition (§8.4: the source's AC-3, E-AC-3 or AAC 5.1 copied, others as AC-3 5.1). Third-party AirPlay TVs: stereo. | L2 → AAC | L2 → AAC (AC-3 only passes through to an AVR) | L1 |
| Interlaced H.264 (1080i25, 576i) | L3 + `yadif`. Apple rule 1.14: "All interlaced source content MUST be deinterlaced." The setting "Send as is" gives L2. | L3 + `yadif` | L3 on gen 1/2 | L1 |
| HEVC (TS or HLS upstream) | L2, **fMP4** segments, `hvc1` tag (Apple 1.5; verified locally [PL §1]) | L2 fMP4 | L2 fMP4 on Ultra, Chromecast with Google TV and Streamer; L3 otherwise (never HEVC in TS) | L1 if the sink's protocolInfo lists it |
| MPEG-2 video, VC-1, other video | L3 | L3 | L3 | L1 |
| Catch-up (timeshift slice, TS or HLS) | L2, EVENT playlist | L2 | L2 | L1 |
| VOD MP4 or MKV (anything ffmpeg reads) | L2, EVENT playlist (P1). Faststart H.264/HEVC + AAC MP4 moves to L1 with Range in P2. | L2 | L1 for MP4 (P2) | L1 |
| Local file / ad-hoc URL | P2 (same pipeline reading the file) | P2 | P2 | P2 |
| DRM (KODIPROP) or encrypted | R `Unsupported` | R | R | R |

Because ffmpeg does the repackaging, HEVC, MKV, MP2 and LATM all work in Phase 1. A native segmenter would have had to refuse them until a later phase (Appendix A).

---

## 5. Architecture

### 5.1 Why the seam is picture A's controller

Three seams were considered.

**(a) A separate cast `PlayerHandle` swapped into the registry.** Rejected.
- `AppShared::main_player()`, the MPRIS `TileRouter`, the queue's `PlayerSink` and the agent all hold clones of A's handle and `watch` receiver from startup. A swap leaves them on a dead channel.

**(b) `Route::Cast` at the router.** Rejected.
- It would bypass the controller's zap, copy fallback, `skip_on_zap`, PIN `unlocked` checks, progress and `Ended`.
- Re-implementing those is the main reliability risk.

**(c) Chosen: a remote output inside `Controller`.**
- `load_now_by` hands the `Playable` to a `RemoteOutput` instead of calling `loadfile`.
- Transport commands are forwarded to it.
- Receiver and relay status are translated into the existing `state::Input`s, so `PlayerMachine` stays the single source of truth.
- Nothing that consumes `PlayerHandle` changes.

### 5.2 Threads and tasks

```
UI thread ──PlayerCommand──▶ "player" controller A (OS thread, crossbeam select!)
  (casting card, picker,       │ output: Local(mpv) │ Remote(RemoteOutput)
   chip, settings)             │     ▲ RemoteEvent (crossbeam)      │ RemoteCmd (tokio mpsc)
        ▲                      ▼     │                              ▼
        │ snapshot.cast   PlayerMachine          tokio "tuner-rt" (3 workers): CastService
        │ (watch)                                 ├─ Discovery: mdns-sd daemon thread, on demand only
        └── CastHandle::devices() (per frame)     ├─ Session task (one per cast)
            CastHandle::firewall() (cached 5 s)   │    ├─ AirPlay client: :7000 control conn (one request in flight),
                                                  │    │   event channel (acks), feedback 2 s, position
                                                  │    └─ Relay session
                                                  │         ├─ upstream proxy 127.0.0.1:0  (reqwest + Playable.headers)
                                                  │         ├─ ffprobe / ffmpeg children (tokio::process, PDEATHSIG)
                                                  │         └─ LAN server <lan-ip>:47800 (axum; only while a session exists)
                                                  ├─ Firewall: detection (files + systemctl), Allow/Remove (pkexec child, 120 s cap)
                                                  └─ logind sleep inhibitor (zbus) while casting
```

- There is no new OS thread except mdns-sd's own daemon, and that runs only while browsing.
- The controller thread stays synchronous.
- ffmpeg is a separate process, so a crash in it can't take the GUI down, and libav soname bumps don't force a Tuner rebuild [PL §1(b)].
- The only code that runs as root is the small `tuner-firewall` helper, started through `pkexec` when the user clicks Allow or Remove (§9.4).

### 5.3 Modules and files

```
src/model/cast.rs            (frozen, skeleton) DeviceId, ReceiverKind, CastPhase, CastLevel, CastView,
                             CastErrorKind, CastHints: the value types the player and the UI share
src/player/remote.rs         controller-side contract (CastConnector, RemoteOutput, RemoteCmd, RemoteEvent)
                             + the pure RemoteEvent → state::Input adapter
src/cast/mod.rs              ENABLED, Device, Protocol, Pairing, Usability, CastHandle (cloneable facade), sanitize_name
src/cast/service.rs          CastService { start(rt, deps) / stop() / Drop } (MprisService pattern, platform/mpris.rs:58-113);
                             implements player::remote::CastConnector
src/cast/session.rs          one task per cast: drives Receiver + RelaySession, emits RemoteEvent, escalation, fetch watchdog
src/cast/discovery.rs        trait Discovery; MdnsDiscovery (mdns-sd), NullDiscovery (off_desktop), FakeDiscovery (tests);
                             manual address probe (GET /info)
src/cast/registry.rs         merged device list (stable ids, last seen, versions, addresses private to the crate)
src/cast/receiver.rs         enum Backend (receiver dispatch): connect, load, replace, set_paused, stop, position, events
src/cast/airplay/            features.rs (TXT/`/info` decode, versions), tlv8.rs, srp.rs, hap.rs (keys + ChaCha20-Poly1305
                             framing), pair.rs (pair-setup both variants / pair-verify), bplist.rs, rtsp.rs (HTTP FIFO +
                             RTSP CSeq on one conn, one request in flight), session.rs (SETUP/RECORD/type-130/TEARDOWN,
                             feedback), queue.rs (/command builders per profile, replace), events.rs (event channel,
                             acks, reassembly, decode), position.rs (playbackInfo + estimate), trace.rs (dev transcript),
                             mod.rs (Receiver impl)
src/cast/relay/              mod.rs (Relay API), server.rs (LAN, axum), token.rs, upstream.rs (loopback proxy, head buffer,
                             playlist rewrite, single upstream connection), probe.rs (ffprobe JSON), plan.rs (pure ladder),
                             ffmpeg.rs (args + supervisor + progress), playlist.rs (served-playlist guard, IDR check,
                             playlist views), renditions.rs (the surround multivariant playlist)
src/cast/firewall/           mod.rs (FirewallState, detect, rule and command text), ufw.rs (user.rules parser),
                             allow.rs (pkexec runner, outcomes)
src/bin/tuner-firewall.rs    the root helper (std only): validates, runs one fixed ufw command
src/cast/inhibit.rs          logind sleep inhibitor (zbus)
src/store/cast.rs            cast_identity / cast_device / cast_hint access (writer closures)
src/store/migrations/0014_cast.sql
src/ui/overlays/cast_picker.rs, cast_pair.rs, cast_firewall.rs, cast_offer.rs
src/ui/screens/settings/cast.rs
packaging/polkit/io.github.rthoms334.omatuner.firewall.policy
tests/cast_airplay.rs, tests/cast_relay.rs, tests/cast_app.rs, tests/cast_service.rs, tests/cast_firewall.rs;
tests/support/fake_airplay.rs (#[path], tests/support/mod.rs untouched)
tests/fixtures/cast/  info_appletv_26_6.plist, info_hisense.plist (scrubbed: deviceID, macAddress, serialNumber, pk,
                      pi, psi, gid, btaddr, senderAddress), txt_*.txt, vectors/pyatv_0_18_0.json, ufw/*.rules, ts/*
scripts/cast/pyatv_vectors.py, scripts/cast/pyatv_interop.py   (dev-only, run with `python -I` in a throwaway venv)
THIRD_PARTY_NOTICES.md      pyatv's MIT notice (ported code), installed with the licence
```

The direction of dependencies keeps `player` independent of `cast`:
- the player defines the contract (`player/remote.rs`);
- `cast` implements it;
- both share value types in `model/cast.rs`.

### 5.4 Types (skeleton sketch)

```rust
// model/cast.rs: frozen; shared by player, cast, ui, agent
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId([u8; 16]);   // blake3(protocol ‖ stable id)[..16]; Display = 32 hex. Never an address.
pub enum ReceiverKind { AppleTv, AirPlayTv, Chromecast, Dlna }
pub enum CastPhase { Connecting, Starting, Playing, Paused, Buffering, Seeking, Lost }
pub enum CastLevel { Original /*L1*/, Repackaged /*L2*/, Converted /*L3*/ }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CastErrorKind { Unreachable, PairingRequired, PairingLost, PairingRefused, Firewall,
                         Unsupported, NeedsFfmpeg, CpuTooSlow, StoppedOnTv, ReceiverFailed }
pub struct CastView { pub device: DeviceId, pub name: String /* sanitized */, pub kind: ReceiverKind,
                      pub phase: CastPhase, pub level: Option<CastLevel>, pub behind_live_s: Option<u16>,
                      pub can_seek: bool, pub volume: bool }
pub struct CastHints { pub vcodec: Option<String>, pub acodec: Option<String>, pub channels: Option<u32>,
                       pub interlaced: Option<bool>, pub height: Option<u32> }

// player/mod.rs: pub additions (sign-off)
PlayerCommand::CastTo { device: DeviceId, play: Option<PlayRequest> }   // None = move what plays now
PlayerCommand::StopCasting { play_here: bool }
PlayerSnapshot { …, pub cast: Option<CastView> }                         // None = local
PlayerErrorKind::Cast(CastErrorKind)
ControllerDeps { …, pub cast: Option<Arc<dyn CastConnector>> }          // None in tests / off desktop

// player/remote.rs
pub trait CastConnector: Send + Sync {
    /// Non-blocking: returns at once; the session connects on the runtime and reports Phase/Lost.
    fn open(&self, device: DeviceId) -> Result<RemoteOutput, PlayerError>;
}
pub struct RemoteOutput { pub cmd: tokio::sync::mpsc::UnboundedSender<RemoteCmd>,
                          pub events: crossbeam_channel::Receiver<RemoteEvent>, pub view: CastView }
pub enum RemoteCmd { Load { gen: u64, playable: Arc<Playable>, hints: CastHints },
                     SetPaused(bool), SeekTo(f64), SetVolume(f64), Stop, Close { fast: bool } }
pub enum RemoteEvent { Phase(CastPhase), Level(CastLevel), UpstreamOpened { gen: u64 },
                       UpstreamClosed { gen: u64, at: Instant }, Http { gen: u64, status: u16 },
                       ProviderText { gen: u64, text: String /* redacted */ }, Playing { gen: u64 },
                       Paused(bool), Buffering(bool), Position { gen: u64, pos: f64, dur: Option<f64> },
                       Ended { gen: u64 }, Failed { gen: u64, kind: PlayerErrorKind }, Lost(CastErrorKind) }

// cast/receiver.rs: enum dispatch, no `dyn` async trait
pub enum Backend { AirPlay(airplay::Client), #[cfg(test)] Fake(FakeReceiver) }   // GoogleCast (P2), Dlna (P3)
// methods: connect(&Device, &Credentials) · load(LoadReq) · replace(LoadReq) · set_paused(bool) · stop() ·
//          set_volume(f64) -> Option<()> · position() -> Option<f64> · next_event() -> ReceiverEvent
// LoadReq { url: Secret<String>, live: bool, offset_s: f64 }   // the TV always starts at 0 of the playlist
//                                                              // it is given; offset_s is added to its positions
```

### 5.5 Controller changes (`src/player/controller.rs`)

**Output state and the event loop**
- `enum Output { Local, Remote(RemoteOutput) }` sits next to the `Mpv` handle.
- `run()` gets a fifth `select!` arm, `recv(remote.events)`, which is `never()` while the output is local.

**`CastTo { device, play }`**
1. Refuse if multi-view is on. `route` already prevents this; the check here is defensive.
2. If something plays locally and `play` is `None`, read the hints from mpv before stopping it: track codecs, `demux-channel-count`, `video-params/h` and `video-frame-info/interlaced`. mpv knows interlace only for the frame being played, and has no level field [PL change 4]. For the Apple TV, level doesn't matter (≤ High@5.2 covers broadcast H.264).
3. `remote = deps.cast.open(device)?` sets `output = Remote` and `snapshot.cast = Some(view)` (phase Connecting).
4. Stop local mpv through the existing `Effect::StopMpv` and `must_stop_first` path (it waits ≤ 1.5 s for END_FILE), so the lease is free.
5. Feed `Input::Play` for `play`, or for the current item with `start_at = snap.position` (VOD and catch-up) or the same channel (live).

**`load_now_by` (`:485`)**
- Budget `acquire(TileId::A, source)` and `register_secret` on every extra header value are unchanged.
- With a remote output it then sends `RemoteCmd::Load { gen }` and feeds `Input::Loaded { entry: Some(gen) }`.

**Releasing the lease**
- `RemoteEvent::UpstreamClosed { gen, at }` releases the lease and records `released`, exactly as END_FILE does.
- So the 45 s race window covers relay reconnects too.

**`StopCasting { play_here }`**
1. Send `RemoteCmd::Close { fast: false }`.
2. Wait ≤ 1.5 s for `UpstreamClosed`.
3. Set `output = Local`.
4. Then either `Input::Play` locally (VOD and catch-up at `last_position`, live at the live point), or `Input::Stop`.

**`Stop` while casting** ends casting. The TV stops, the session closes and the output goes back to Local. So the sleep timer and Ctrl+Shift+S turn the TV off. The picker keeps the device first in its list for next time.

**`Lost(StoppedOnTv)`** (stopped from the TV's remote, or another sender took over) ends casting the same way. It does so without an error banner: the toast or notification from §3.4 offers Play here and Cast again.

**`command()` (`:664`)** branches on output for transport (§11.3).

**`publish`** sets `snapshot.cast`. While casting, these fields stay at their defaults or empty:
- `tracks`, `stream_info`, `hwdec`, `video_size`, `cache_secs`, `video_suspended`, `sub_*`, `aspect`, `zoom`.

The exception is `stream_info` while the info panel is open, which is filled from the probe.

**`state.rs` `MachineConfig`** gets remote timeouts (sign-off: frozen skeleton body). The reconnect policy itself is unchanged.
- **Load: 30 s.** That covers upstream connect, the probe, the first segment and AVPlayer's start. foxCast also gives up after 30 s without `playing` [REF].
- **Stall: 20 s.**

**`playback/route.rs`**
- `RouteState` gets `casting: bool`.
- Plays aimed at picture B, and Ctrl+M "Alongside", return `Refuse("Stop casting to use multi-view")`.
- Plays aimed at A go through as today, and so land on the TV.

### 5.6 Service lifecycle (`src/services.rs`)

**Start and settings changes**
- `Integrations` gains `cast: Option<CastService>`.
- It is started in `start_integrations` (`:155-217`) through `ok()`, so a failure means no casting, never a crash.
- `apply_config` (`:289-353`) restarts it on `[cast]` changes using the mpris pattern (`.take().stop()`, then start if enabled).
  - Turning casting off while picture A casts sends A `StopCasting { play_here: true }` first: the picture comes back here, instead of the session's end reading as a lost TV and every later play trying to reopen it.
- `AppShared::set_cast(CastHandle)` exposes it to the UI and the agent (the `set_queue_handle` slot pattern).

**Shutdown (`:408-433`)**
- `cast.stop()` runs before `integrations = Integrations::default()`.
- It does the following:
  - sends a fire-and-forget stop to the TV (300 ms cap);
  - kills the children, including a `pkexec` still waiting, which closes its dialog;
  - closes the listeners;
  - drops the inhibitor;
  - deletes the session directories.
- All of this fits the runtime's 2 s shutdown budget.

**Signal exit (`exit_cleanup`, `:388-405`)**
- It takes the same fast path.
- Children are spawned with `PR_SET_PDEATHSIG = SIGKILL` (`pre_exec`), so ffmpeg dies even if Tuner is SIGKILLed.
- **Spawn them from a runtime worker, never the blocking pool.** PDEATHSIG follows the spawning *thread*, and blocking-pool threads exit when idle.
- Once the relay is gone, the TV stalls. That is the hard kill switch.

### 5.7 Events and shared state

- **`msg.rs` (frozen) is not touched.**
  - Session state reaches the UI through `PlayerSnapshot.cast`.
  - Failures go through the existing `PlayerEvent::Failed` and `AppEvent::Toast`, with copy that is already redacted. The app picks the card, the toast or a notification (§3.8).
  - The device list is read from `CastHandle::devices()` every frame, as `agent_connect` reads its rows.
  - The first-run pass uses the `take_pass_outcome()` polling pattern (`app.rs:328-368`).
  - Allow and Remove report through `CastHandle::firewall_job()`, polled per frame like the pass.
- **Parental view** is read at request time from `AppShared::parental(cfg)`, never cached.

---

## 6. Discovery (`src/cast/discovery.rs`)

**Library: `mdns-sd`, pinned `=0.21.4`.**
- It co-binds 5353 with avahi and Chromium using `SO_REUSEADDR`/`SO_REUSEPORT` (verified in its source [NE B.3]).
- As a browser it queries and receives by multicast, which ufw's `before.rules` allow.
- No avahi dependency.
- Phase 1 browses `_airplay._tcp.local.`. Phase 2 adds `_googlecast._tcp`.

**When it runs: on demand only.**
- while the picker or Settings ▸ Cast is open;
- for the 5 s first-run pass;
- for a 2 s refresh before casting to a remembered device.

There is no continuous scanning. Under `platform::off_desktop()` it uses `NullDiscovery`, so tests never touch the LAN.

**Classification (`airplay/features.rs`), using TXT records or `/info`**
- Read `features` (or `ft`) as `lo,hi` 32-bit words, and `flags` (or `sf`).
- The Apple TV's `/info` `statusFlags` has bit 20 set (0x118644) while its TXT `flags` does not (0x18644). The decoder accepts both [FAKE].
- Video-capable means **bit 0 or bit 49**, the gate pyatv uses. The Hisense has only bit 49.
- Pairing is required if `flags & (0x200 | 0x8)`. Never gate on 0x8 alone: the tvOS 27 unit had it clear while needing a PIN [AP 1.6].
- Unsupported (listed but greyed out):
  - `act=2`, or a model matching `^Mac\d+,\d+$`: "Can't cast to Macs";
  - `acl=1`: "Same Home only", with the Allow Access hint.
- Integrator other than Apple (a `model` not starting with `AppleTV`): a third-party AirPlay TV, cast with the third-party profile (§7.7).
  - It also needs features bit 33, which gates play-queue support for third-party TVs (VioletRelay) [REF].
  - It is listed, and offered by the first-run card, like an Apple TV. There is no experimental tier (§17 item 9).
- **Versions** are read for logs, `diagnose` and Settings only (§7.6):
  - TXT `osvers` (26.6) and `srcvers` (960.13.1);
  - `/info` `osBuildVersion` (23L773) and `sourceVersion`.

**Identity**
- `DeviceId = blake3("airplay" ‖ pi-or-deviceid)[..16]`.
- Addresses are re-resolved every session and never used as ids.
- The last address is kept in the store, for manual devices and quick reconnects.

**Merging (Phase 2+).** One physical TV offering several protocols shows as one row. The preference order is:
- Apple → AirPlay;
- others → Cast > DLNA > third-party AirPlay [DL §5].

**Untrusted strings.** Names, models and versions are sanitized: control characters stripped, capped at 64 characters, and any `://` makes the name "AirPlay TV". They are redacted in logs and never logged at INFO with an address.

**Manual "Add by address…".** IPv4 entry, then `GET http://<ip>:7000/info` (unauthenticated [NE]), then the same classification.

---

## 7. AirPlay client (`src/cast/airplay/`)

The working video flow on tvOS 26 and 27 is reverse-engineered, and pyatv has not merged it. It is still proven, though. Several open-source senders play URL video on real Apple TVs with it:
- pyatv PR 2899 on tvOS 26.5;
- robkochman's pyatv branch on 26.5;
- castcli on AppleTV14,1 with tvOS 26.6, the user's exact model and OS;
- VioletRelay on 26.5 and on a Hisense VIDAA;
- foxCast on tvOS 27.

The client follows them step by step (§7.0):
- Pairing, verify, framing and the event-channel crypto are **ported** from pyatv, which is MIT-licensed and has been released for years.
- The play-queue sequence follows the senders that have hardware evidence.
- Where they disagree, this section picks one and says why. Where a choice could matter on the user's TV, the client takes a tolerant path rather than a guess. Examples are the non-fatal extras, the `skipRecord` handling and playlists that always start at 0.

### 7.0 Reference implementations (what each step is ported from)

**Pinned sources**

| Source | Commit | Licence | Use | Hardware evidence |
|---|---|---|---|---|
| pyatv `master` (Release 0.18.0) | [`b277a4c8`](https://github.com/postlund/pyatv/tree/b277a4c8222ecdcbaab8a24e3e713ca44765adb4) | MIT, © 2020 Pierre Ståhl | **ported code**: pairing, verify, framing, mux, event crypto | years on tvOS ≤ 26 (remote control) |
| pyatv PR [2899](https://github.com/postlund/pyatv/pull/2899) (open) | head [`b2484099`](https://github.com/postlund/pyatv/tree/b24840990af7ae5505538d36fc42cef7b3cc348e) | MIT (pyatv contribution) | SETUP PTP, event dispatch, quirks | tvOS 26.5: PTP vs NTP measured |
| robkochman `codex/play-url-tvos26` | [`8144c77c`](https://github.com/robkochman/pyatv/commit/8144c77c6cecbed4f9ba2adb5a350ad86a8f6604) | MIT | `/command` sequence + its tests | tvOS 26.5: remote HLS, local HLS, MP4 |
| pyatv PR [2846](https://github.com/postlund/pyatv/pull/2846) (open) | head [`8848ad3f`](https://github.com/postlund/pyatv/tree/8848ad3fd9ae46b8eb733bfc667b536a28f04c5a) | MIT | `playbackInfo` request/response | author: Apple TV + Samsung Frame, no transcript |
| pyatv PR [2894](https://github.com/postlund/pyatv/pull/2894) (open) | head [`fa9ef01d`](https://github.com/postlund/pyatv/tree/fa9ef01dce9928e6be959b7f9b67d14a0c722b06) | MIT | `X-Apple-Client-Name` + its fake-receiver test | tvOS 27, incl. AppleTV14,1 (A2843) |
| foxCast | [`2633525a`](https://github.com/Doridian/foxCast/tree/2633525a7b6a87ee98758aabfd41bbaec67e475d) | LGPL-3 | **facts and test scenarios only** | AppleTV11,1 tvOS 27 (24J361), PTP play queue |
| VioletRelay | [`07b13d16`](https://github.com/ruhbyook/VioletRelay/tree/07b13d16f0aebbf826419dc86bb9b46e1e0ce79d) | GPL-3 | **facts only** | Apple TV 4K tvOS 26.5, Hisense 100E75LE, Sony |
| castcli | [`340d1202`](https://github.com/agustif/castcli/tree/340d120209efdd2a87952e6cae4571811d6d514d) | none published | **facts only**, nothing copied | **AppleTV14,1 tvOS 26.6** (issue 35) |
| doubletake | [`5e2f1cbe`](https://github.com/omarroth/doubletake/tree/5e2f1cbe3e1070bcefec8e549c7baf884727bc00) | LGPL-3 | facts only (tvOS 27 pairing, frames) | AppleTV11,1 and 14,1 on tvOS 27 (mirroring) |
| LIVI | [`dfb487b4`](https://github.com/f-io/LIVI/tree/dfb487b4207733151b43e91e0cd2c292524c999f) | GPL-3 | facts only (iOS-side `playbackInfo` reply shape) | CarPlay receiver, not tvOS |
| PearPlay | `fc36428` | MIT | firewall helper pattern | — |
| Apple HLS Authoring Specification | — | — | rules 1.5, 1.14, 2.3, 7.4, 7.5, 8.4, 8.8, 8.11, 8.18 | — |

**Licence policy**
- **pyatv (MIT) is ported faithfully.**
  - Each ported file starts with `// Ported from pyatv <path> @ b277a4c8 (MIT, © 2020 Pierre Ståhl).`
  - The MIT notice goes in `THIRD_PARTY_NOTICES.md`, installed beside Tuner's licence.
  - The open PRs 2899, 2846 and 2894 and robkochman's branch are contributions to pyatv under the same licence, credited the same way.
- **foxCast and doubletake (LGPL-3), and VioletRelay, LIVI and DiPlay (GPL-3), are read for protocol facts only.** That means header and key names, constants, step order, error codes and test scenario lists. No code is translated from them.
  - Tuner is GPL-3.0-or-later, so translating "-3-only" code would pin the combined work to version 3 and add attribution duties for little gain.
  - Protocol facts aren't copyrightable.
- **castcli, `openairplay/ap2-sender` and `openairplay/airplay-spec` publish no licence.** They are read for facts only, and nothing from them is committed.

**Step by step**

| Step | Port from (permalink) | Merged upstream? |
|---|---|---|
| TLV8 | pyatv [`pyatv/auth/hap_tlv8.py`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/auth/hap_tlv8.py) (tags 0x00–0x0D, 0x11 Name, 0x13 Flags; add 0x12 for the HKP 5 ACL) | released |
| Pair-setup, HKP 3 | pyatv [`protocols/airplay/auth/hap.py#L35-L95`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/protocols/airplay/auth/hap.py#L35-L95) + [`auth/hap_srp.py#L138-L231`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/auth/hap_srp.py#L138-L231); headers from PR 2894; the M4/M6 checks pyatv leaves as TODO from foxCast [`pairing.go#L561-L700`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/internal/sender/pairing.go#L561-L700) and [`docs/03-authentication.md`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/docs/03-authentication.md) | released; client name in open PR 2894; checks: foxCast `main` |
| Pair-setup, HKP 5 + ACL | foxCast [`pairing.go#L100-L217`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/internal/sender/pairing.go#L100-L217) (headers, `X-Apple-SupportedPINLengths`, the ScreenCapture ACL bytes); doubletake [`5f14b6f`](https://github.com/omarroth/doubletake/commit/5f14b6f96301b661ac92b6752b06044986d30f86) "Fixes for tvOS 27.0" | foxCast/doubletake `main` |
| Pairing on one connection | castcli [`AirPlayPairHttp.ts#L1-L6`](https://github.com/agustif/castcli/blob/340d120209efdd2a87952e6cae4571811d6d514d/apps/cli/src/AirPlayPairHttp.ts#L1-L6) (comment: pair-setup is authorised on the pin-start socket) | castcli `main` |
| Pair-setup error TLVs | pyatv PR [2920](https://github.com/postlund/pyatv/pull/2920) (Error/BackOff with HTTP 200); foxCast [`pairing.go#L410-L416`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/internal/sender/pairing.go#L410-L416) (453 on pin-start is success) | open PR |
| Pair-verify + control keys | pyatv [`hap.py#L97-L151`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/protocols/airplay/auth/hap.py#L97-L151), [`hap_srp.py#L66-L136`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/auth/hap_srp.py#L66-L136), [`airplay/auth/__init__.py#L100-L120`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/protocols/airplay/auth/__init__.py#L100-L120) | released |
| Frame cipher | pyatv [`auth/hap_session.py`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/auth/hap_session.py) + [`support/chacha20.py`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/support/chacha20.py) | released |
| HTTP/RTSP mux | pyatv [`support/rtsp.py#L254-L330`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/support/rtsp.py#L254-L330) (`exchange`, RTSP headers) + [`support/http.py#L436`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/support/http.py#L436); a stray-late-reply guard added for issue [#2893](https://github.com/postlund/pyatv/issues/2893) | released (guard is new) |
| SETUP (PTP), RECORD, `/info`, type 130 | PR 2899 [`airplayv2.py#L232-L460`](https://github.com/postlund/pyatv/blob/b24840990af7ae5505538d36fc42cef7b3cc348e/pyatv/protocols/raop/protocols/airplayv2.py#L232-L460); same sequence in foxCast [`playback.go#L205-L360`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/internal/sender/playback.go#L205-L360) | open PR; foxCast `main` |
| Event channel keys | pyatv [`auth/hap_channel.py#L79-L99`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/auth/hap_channel.py#L79-L99) (`Events-Salt`, swapped keys) | released |
| Event ack + reassembly | foxCast [`event_channel.go#L307-L350`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/internal/sender/event_channel.go#L307-L350) (behaviour); castcli issue [35](https://github.com/agustif/castcli/issues/35) (1024 + 1259-byte split) | — |
| Event decoding | PR 2899 [`channels.py#L60-L135`](https://github.com/postlund/pyatv/blob/b24840990af7ae5505538d36fc42cef7b3cc348e/pyatv/protocols/airplay/channels.py#L60-L135); robkochman `_handle_event` (the `params.playbackState` form); foxCast [`docs/05`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/docs/05-video-url-playback.md) (state and notification names) | open PR / branch |
| `/command` play queue | foxCast [`playback.go#L362-L412`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/internal/sender/playback.go#L362-L412) (item keys, headers); robkochman [`8144c77c`](https://github.com/robkochman/pyatv/commit/8144c77c6cecbed4f9ba2adb5a350ad86a8f6604) (`actionAtItemEnd = 1`, non-2xx stops the sequence) | foxCast `main`; branch |
| `playbackInfo` (position) | PR 2846 [`raop/protocols/airplayv2.py`](https://github.com/postlund/pyatv/blob/8848ad3fd9ae46b8eb733bfc667b536a28f04c5a/pyatv/protocols/raop/protocols/airplayv2.py) `playbackInfo` + [`airplay/channels.py`](https://github.com/postlund/pyatv/blob/8848ad3fd9ae46b8eb733bfc667b536a28f04c5a/pyatv/protocols/airplay/channels.py) `responseFor`; reply keys from LIVI [`video_playback.rs#L157-L236`](https://github.com/f-io/LIVI/blob/dfb487b4207733151b43e91e0cd2c292524c999f/native/livi-helperd/crates/livi-cp/src/video_playback.rs#L157-L236) | open PR |
| Replace (zap, seek, reconnect) | VioletRelay [`airplay_modern.py#L429-L500`](https://github.com/ruhbyook/VioletRelay/blob/07b13d16f0aebbf826419dc86bb9b46e1e0ce79d/src/violetrelay/services/airplay_modern.py#L429-L500) `replace` | VioletRelay `main` |
| Stop, end of media | VioletRelay (`setRate 0` + TEARDOWN); foxCast [`playback.go#L579-L612`](https://github.com/Doridian/foxCast/blob/2633525a7b6a87ee98758aabfd41bbaec67e475d/internal/sender/playback.go#L579-L612) (`waitQueue`) | `main` |
| Third-party profile | VioletRelay [`airplay.py#L963-L971`](https://github.com/ruhbyook/VioletRelay/blob/07b13d16f0aebbf826419dc86bb9b46e1e0ce79d/src/violetrelay/services/airplay.py#L963-L971), [`airplay_modern.py#L84-L300`](https://github.com/ruhbyook/VioletRelay/blob/07b13d16f0aebbf826419dc86bb9b46e1e0ce79d/src/violetrelay/services/airplay_modern.py#L84-L300) | `main` |
| Segments ready before play | VioletRelay [`converter.py#L205-L228`](https://github.com/ruhbyook/VioletRelay/blob/07b13d16f0aebbf826419dc86bb9b46e1e0ce79d/src/violetrelay/services/converter.py#L205-L228) (`wait_until_ready(segments=2)`, 4 s segments) | `main` |
| Firewall helper | PearPlay `helper/firewall.py` (`fc36428`): a scoped ufw rule via pkexec, with an ownership comment | `main` |
| Fake receiver | pyatv [`protocols/airplay/server_auth.py`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/protocols/airplay/server_auth.py), [`tests/fake_device/airplay.py`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/tests/fake_device/airplay.py), [`docs/documentation/protocols.md`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/docs/documentation/protocols.md) L1525–1979; see §14.3 | released |

**What is merged and released, and what is not** [REF]

| Item | State |
|---|---|
| HAP pair-setup and pair-verify, framing, event-channel crypto, `/feedback` | Merged and released (pyatv 0.18.0). Proven for years on tvOS ≤ 26.x for remote control. |
| The `/command` play-queue video flow | Not merged in pyatv: PRs 2899 and 2846 are open, and robkochman's is a branch only. Shipping in foxCast and VioletRelay. Implemented in castcli. |
| `X-Apple-Client-Name` for tvOS 27 pairing | Not merged (PR 2894). Merged in the Unfolded Circle integration (PR 165) and shipped in foxCast and doubletake. |
| Pair-setup error-TLV checks | Not merged (PR 2920). |
| Seek and volume over a control-only session | Not merged (PR 2919), transient pairing only. Out of Phase 1. |

### 7.1 Crypto and transport

**Crypto: no new crypto crates.** `aws-lc-rs` is already compiled for rustls and has a ring-compatible API. It provides:
- X25519 (ephemeral);
- `Ed25519KeyPair::from_seed_unchecked` and Ed25519 verify;
- ChaCha20-Poly1305;
- HKDF-SHA512;
- SHA-512;
- `SystemRandom`.

**SRP-6a (`srp.rs`)**
- About 150 lines, hand-rolled on `num-bigint`. It is ported from pyatv's `hap_srp.py` behaviour (srptools), and the maths was cross-checked against foxCast's explicit implementation [REF].
- RFC 5054 3072-bit group, g = 5, SHA-512, identity `"Pair-Setup"`, password = the 4-digit PIN string.
- `x = H(salt ‖ H("Pair-Setup:" ‖ pin))`.
- `k = H(N ‖ PAD(g))` and `u = H(PAD(A) ‖ PAD(B))` are padded to 384 bytes.
- **The M1 proof uses unpadded values:** `H(H(N) ⊕ H(g) ‖ H(I) ‖ salt ‖ A ‖ B ‖ K)` with `K = H(S)`.
  - A padded `H(g)` gives a 470 at M3 that looks like a policy refusal (AirPlayDroid) [AP 1.8], [TV27].
- The server proof `H(A ‖ M1 ‖ K)` **is verified**. pyatv leaves this as a TODO; foxCast checks it.
- This avoids pinning `srp =0.7.0-rc.3` and depending on its `#[doc(hidden)]` `utils`.
- The server half (salt, verifier, `B`, `S`) is in the same module, `#[doc(hidden)] pub`, for the fake receiver (§14.3).

**TLV8 and bplist00**
- Both are hand-rolled.
- TLV8 joins fragmented values (more than 255 bytes) and keeps unknown tags. Readers ignore tags they don't know, so a type-17 TLV of 159 bytes in M6 (seen on 26.6, send-airplay2 PR 8) never fails a pairing [TV27].
- The bplist reader caps size, depth and offsets, because its input is untrusted.
- The `plist` crate would bring `time` and possibly a second `quick-xml`.

**Framing (`hap.rs`, ported from pyatv `hap_session.py`)**
- Each frame is a 2-byte little-endian plaintext length (also the AAD), then the ciphertext, then a 16-byte tag.
- The nonce is 4 zero bytes plus a little-endian u64 counter, one per direction.
- **Outgoing** plaintext is at most 1024 bytes (pyatv).
- **Incoming** frames may be any length the header allows, up to 65 535. pyatv's decrypt already accepts that, and doubletake's reader accepts 16 KiB.
  - The "frames over 1024 bytes on tvOS 27" claim has no public evidence [REF], [TV27]. Leniency costs nothing.
  - Hardening caps: 64 KiB per frame and 1 MiB per reassembled message.

**Transport (`rtsp.rs`, ported from pyatv `rtsp.py`/`http.py`)**
- One TCP connection to `:7000` carries both HTTP/1.1 and RTSP/1.0.
- **Exactly one request is in flight at a time.** PR 2899 and foxCast both report that an RTSP `/feedback` racing an HTTP `/command` makes the receiver drop the connection.
- RTSP replies are matched by `CSeq`, and HTTP replies in order (FIFO).
  - A reply whose CSeq matches nothing pending, such as a late `/feedback` answer (pyatv #2893), is dropped and logged at debug. It never desynchronises the matching.
- RTSP requests carry `CSeq` (from 0), `DACP-ID`, `Active-Remote`, `Client-Instance` and `User-Agent: AirPlay/550.10` (pyatv).
- Encryption is switched on **immediately** after pair-verify. foxCast notes that the TV drops the connection about 1 ms after a plaintext request.
- All I/O has timeouts: connect 3 s, request 5 s.

### 7.2 Pairing (one time per TV)

```
Tuner                                                     Apple TV :7000   (ONE TCP connection for all of it)
POST /pair-pin-start   X-Apple-Client-Name: Tuner + variant headers ──▶ shows a 4-digit PIN (200; 453 is also success)
        (user types it into Tuner's cast_pair overlay; the connection stays open)
POST /pair-setup M1 {Method 0, State 1}                   ─────▶
                                                     ◀───── M2 {State 2, Salt, PublicKey B} │ {Error, BackOff}
SRP-6a (3072, g=5, SHA-512; padded k and u; unpadded M1 proof)
POST /pair-setup M3 {State 3, PublicKey A, Proof}         ─────▶
                                                     ◀───── M4 {State 4, Proof} │ {Error 2: wrong PIN}   (proof verified)
POST /pair-setup M5 {State 5, EncryptedData{Identifier, PublicKey LTPK, Signature, 0x11 Name [, 0x12 ACL]}} ─────▶
                                                     ◀───── M6 {State 6, EncryptedData{Identifier, PublicKey, Signature, …}}
                                                            (signature verified; unknown TLVs ignored)
store: cast_identity (once per install) · cast_device.peer_id + peer_ltpk + pair_kind + paired_at
```

**Two variants, tried in this order**

| | **HKP 3** (first) | **HKP 5 + ScreenCapture ACL** (fallback) |
|---|---|---|
| Headers on `/pair-pin-start` and every `/pair-setup` | `X-Apple-HKP: 3`, `X-Apple-Client-Name: Tuner`, `Content-Type: application/octet-stream`, `User-Agent: AirPlay/320.20` | `X-Apple-HKP: 5`, `X-Apple-Client-Name: Tuner`, `X-Apple-Client-ID: <pairing UUID>`, same Content-Type and User-Agent. `/pair-pin-start` adds `X-Apple-SupportedPINLengths: 4`. |
| M5 extra | TLV 0x11 Name = OPACK `{"name": "Tuner"}` (`E1 44 "name" 45 "Tuner"`): pyatv's AirPlay pairing handler always passes its name to `step3`, and PR 2894 sends the same name as `X-Apple-Client-Name` | the same Name, then TLV 0x12 = `E1 57 "com.apple.ScreenCapture" 01` (OPACK `{"com.apple.ScreenCapture": true}`), in pyatv's order (name, then `additional_data`) |
| `/pair-verify` headers | `X-Apple-HKP: 3`, client name | the same as pair-setup, plus `X-Apple-PD: 1` |
| Evidence | castcli: AppleTV14,1, tvOS 26.6, video played. PR 2894: PIN shown on 14,1 with tvOS 27. | foxCast: AppleTV11,1, tvOS 27, URL video. doubletake: tvOS 27. |
| Source | pyatv `hap.py` + PR 2894 | foxCast `pairing.go` (facts) |

- **The ladder.** HKP 5 is tried once when HKP 3 runs into any of these:
  - a 470 at M2–M4;
  - a `/pair-pin-start` status other than 200 or 453;
  - the user pressing **Show a new code**.
  
  The variant that worked is stored as `cast_device.pair_kind`, and pair-verify uses its headers.
- **`X-Apple-Client-Name` is sent from day one** on `/pair-pin-start` and on every `/pair-setup`, in both variants.
  - On tvOS 27 the PIN UI bails without it: the TV logs `passcodeString is empty or clientName is nil. Bailing. {clientNameIsNil=YES}` (pyatv #2866, PR 2894) [TV27].
  - It is harmless on 26.
- **One connection.** Pin-start and the whole pair-setup run on one TCP connection. castcli found that "Apple TV authorizes pair-setup on the socket that called pair-pin-start", and a new connection per request got 470.
  - This is also a likely cause of the 470-after-PIN report on a 14,1 with 26.5 (pyatv #2866).
  - The overlay holds the connection while the user types, and closes it after M6 or Cancel.
- **Keys and labels** (pyatv `hap_srp.py`):
  - encryption key `HKDF-SHA512(K, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info")`, with nonces `PS-Msg05` and `PS-Msg06`;
  - controller signing input `HKDF(K, "Pair-Setup-Controller-Sign-Salt", "Pair-Setup-Controller-Sign-Info") ‖ id ‖ LTPK`;
  - the accessory's M6 signature is verified over `HKDF(K, "Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info") ‖ TV id ‖ TV LTPK` (foxCast).
  - The pairing id is a per-install UUID string, `cast_identity.pairing_id`.
- **Replies.** M2 and M4 can carry an `Error` TLV with HTTP 200, so it is checked first. Unchecked, it turned into a 10 s SETUP hang (PR 2920).
  - `Error 2` at M4 → wrong PIN.
  - `Error 3` → backoff, honouring `BackOff` seconds.
  - Any other error → `PairingRefused`.
  - HTTP 470 → the ladder, then `PairingRefused` with the Allow Access hint.
  - 403 at pin-start (a Mac, `act=2`) → unsupported.
- **Transient (PIN-less) pairing is not implemented.** A receiver with bit 9 refuses it with 470 at M3 (AirPlayDroid), and both of the user's TVs set bit 9 [AP 1.4], [NE 0.1].
- **Later failures.** If pair-verify fails later, the TV was reset, re-paired, or Allow Access changed, which major updates can do. That yields `PairingLost`, which leads to Pair.
- Phase 4 adds Companion-credential pairing, but only if HAP PIN pairing fails on the user's TV.

**Pair-verify (every session, ported from pyatv `hap.py` + `hap_srp.py`)**
- M1 `{State 1, PublicKey: X25519 ephemeral}`.
- M2 `{PublicKey, EncryptedData}`:
  - key `HKDF-SHA512(shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info")`, nonce `PV-Msg02`;
  - the inner `Identifier` must equal the stored `peer_id`;
  - the `Signature` is verified with `peer_ltpk` over `serverPub ‖ id ‖ clientPub`.
- M3 `{State 3, EncryptedData}`, nonce `PV-Msg03`, signing `clientPub ‖ clientId ‖ serverPub`.
- The M4 status is checked. pyatv leaves this as a TODO.
- Control keys: `HKDF(shared, "Control-Salt", "Control-Write-Encryption-Key")` for our writes, `"Control-Read-Encryption-Key"` for our reads.
- Failures:

  | Reply | Result |
  |---|---|
  | An empty or missing body with 200 | `PairingRefused` with the "Anyone on the Same Network" hint. On tvOS 27.2 "Everyone in Your Home" did exactly this (pyatv #2885) [TV27]. |
  | Id or signature mismatch, or an Error TLV | `PairingLost` |
  | 470 | `PairingRefused` |
  | Refused or timed out | `Unreachable` |

### 7.3 Session (per cast)

The sequence is PR 2899's and foxCast's, which agree on it. foxCast has hardware evidence on tvOS 27 and PR 2899 on 26.5. It differs from them only where noted.

```
1  TCP :7000 → POST /pair-verify (variant headers) → control keys; encryption on at once
2  SETUP rtsp://<lan-ip>/<random u32> (base, bplist):
     timingProtocol "PTP"; timingPeerInfo {ID: <session UUID>, Addresses: [<lan-ip>], DeviceType 0,
     SupportsClockPortMatchingOverride true}; timingPeerList [same]; sessionUUID; sessionCorrelationUUID;
     updateSessionRequest false; statsCollectionEnabled false; isMultiSelectAirPlay false;
     deviceID = macAddress = cast_identity.sender_mac (random, locally administered); name "Tuner";
     model "iPhone14,3", osName "iPhone OS", osVersion "16.5", osBuildVersion "20F66", sourceVersion "690.7.1"
   → eventPort [, skipRecord]
3  TCP eventPort (connect retried 5× 1 s apart, pyatv); keys HKDF(shared, "Events-Salt", …) SWAPPED:
   read with "Events-Write-Encryption-Key", write with "Events-Read-Encryption-Key".
   Every RTSP-framed request (POST /command updateInfo, playbackState, notification…) is acked
   "RTSP/1.0 200 OK" + its CSeq + Content-Length: 0, else the TV closes the session after ~25–30 s;
   requests are reassembled across HAP frames up to Content-Length and acked once (castcli #35)
4  RECORD (Range: npt=0-, RTP-Info: seq=0;rtptime=0), skipped when SETUP said skipRecord=true;
   a RECORD error is logged and the session goes on
5  GET /info HTTP/1.1 → psi; model, osBuildVersion, sourceVersion (logged for diagnose)
6  SETUP {streams:[{type 130, controlType 1, channelID "<psi>-RCS-1", clientUUID <UUID>,
   clientTypeUUID "A6B27562-B43A-4F2D-B75F-82391E250194"}]} → streams[0].streamID
   (no dataPort, nothing connects; it only registers. /command answers 500 until it exists)
7  POST /command HTTP/1.1, Content-Type: application/x-apple-binary-plist, X-Apple-StreamID: <streamID>,
   body bplist {params: {data: <bplist of the command>}}:
     insertPlayQueueItem {item}                                      fatal on non-2xx: nothing more is sent
     setProperty isInterestedInDateRange true {item: {uuid}}         non-fatal
     setProperty actionAtItemEnd 1                                   non-fatal
     setRate 1.0                                                     fatal (else the item loads paused)
8  POST /feedback RTSP/1.0 every 2.0 s, starting only after step 7, never in flight with a /command
9  events: playbackState / notification → phase, Ended; position from playbackInfo or an estimate
```

**Why each choice**
- **Timing: declared PTP only, with no clock, no inbound UDP and no knob.**
  - PR 2899 measured tvOS 26.5. With PTP the picture was still playing at 60 s with 26 events. With NTP there were zero events and the picture was gone by about 27 s, even though the media was fully fetched.
  - foxCast found the same on tvOS 27: "with NTP the item plays but no events arrive and the session dies after ~20 s".
  - VioletRelay also declares PTP.
  - foxCast's real PTP follower exists only for HomePod audio; its URL playback only declares PTP.
  - The one counter-example is castcli's NTP responder flow on 14,1 with 26.6. It reports playback starting, not how long it lasted.
  - So the NTP responder and `timing_port` are dropped. §16 risk 2 covers the case where declared PTP misbehaves on 26.6.
- **Order: RECORD, then `GET /info`.** This is the order of PR 2899, foxCast and castcli. robkochman reads `/info` first, with NTP and early feedback.
  - PR 2899 notes that reading `/info` before pair-verify, or starting feedback before RECORD, drops the connection.
- **Feedback after the commands.** PR 2899 and foxCast (`playback.go#L240-L246`) both do this. VioletRelay starts feedback earlier and still works, so the strict order costs nothing.
  - The interval is 2.0 s, pyatv's `FEEDBACK_INTERVAL` ("This is what iOS uses").
  - Feedback is best effort. The event-channel ack is the real keep-alive.
- **`skipRecord`** has been seen only in mirroring SETUP replies (foxCast `mirror.go#L579-L588`, doubletake). Neither foxCast nor pyatv reads it for video, and foxCast always sends RECORD on tvOS 27.
  - So: honour it when present, send RECORD otherwise, and treat a RECORD error as non-fatal.
- **The item** (Apple TV) is foxCast's key set:

  ```
  uuid <UUID, upper case> · Content-Location http://<lan-ip>:47800/c/<tok>/index.m3u8 · mediaType "file" ·
  IsTLSEnabled false · playbackRestrictions 0 · referenceRestrictions 2 · supportsIntegratedTimeline false ·
  snapTimeToPausePlayback false · clientBundleID "io.github.rthoms334.tuner" · clientProcName "Tuner" ·
  playerLoggingID "TUNER" · playerItemLoggingID "I/TUNER.01" · Start-Position {flags 1, value 0, epoch 0, timescale 1}
  ```

  - **`playerLoggingID` is at most 6 characters.** Longer values make the TV close the connection silently. `P/ABCD` works and `P/ABCDE` doesn't (PR 2899).
  - **The start position is always 0.** Every item's playlist starts where playback should start: a `-ss` restart, or a playlist view (§8.5). So Tuner never depends on whether tvOS honours a start key.
    - The references disagree on the key: a `Start-Position` dict in PR 2899, foxCast and VioletRelay; `Start-Position-Seconds` in robkochman and castcli.
    - Screenbox reports that tvOS 26 ignores the start position.
- **`actionAtItemEnd = 1`** is sent by robkochman and castcli (26.5, 26.6), and VioletRelay says it is required for URL playback to start on its 26.x hardware. PR 2899 and foxCast omit it.
  - Sending it non-fatally covers both.
  - `isInterestedInDateRange` is sent the same way (castcli and foxCast send it; VioletRelay doesn't).
- **Headers on `/command`** (Apple TV) are exactly foxCast's minimal form, which works on tvOS 27:
  - `Content-Type` and **`X-Apple-StreamID`**, spelt exactly so. VioletRelay found that the hyphenated `X-Apple-Stream-ID` gets a 200 on an Apple TV but the item never starts.
  - No `X-Apple-ProtocolVersion` or `X-Apple-Session-ID`. Apple TVs tolerate them, but they aren't needed, and the Hisense rejects them.
- **`channelID`** is `<psi>-RCS-1` (PR 2899, foxCast), an Apple TV's `/info` `psi`. Without a `psi` it is `<sender MAC>-RCS-1`, castcli's form; both work on 26.x. A third-party TV's comes from its TXT record (§7.7).
- The type-130 SETUP is **not** the MRP tunnel, which is `controlType 2` with `clientTypeUUID 1910A70F-…`. The `A6B27562-…` client type is the "UrlFling" constant, confirmed independently by LIVI and DiPlay.
- **The sender identity strings** are foxCast's. castcli sends the same model and OS with `sourceVersion 960.13.1`. Every working sender presents itself as an iPhone.

**Replace (zap, seek, live reconnect), from VioletRelay `replace`** [HW on 26.5, Apple TV and Hisense]

```
setRate 0 → setProperty actionAtItemEnd 0 → insertPlayQueueItem {new item, itemAfter: {uuid: old}}
          → removePlayQueueItem {item: {uuid: old}} → setRate 1
… once the TV fetches the new item's first segment: setProperty actionAtItemEnd 1
```

- PR 2899 observes that a replacement goes `playing → loading → playing`, and `stopped` fires only when nothing plays any more.
- **Any error in the sequence** leads to TEARDOWN and the whole session again (pair-verify … insert). That takes about 2 s longer, but it is exactly the proven start path.
- Re-pairing is never needed for a zap.

**Stop.** `setRate 0`, then `TEARDOWN rtsp://…`, then the event channel and control connection close (VioletRelay; foxCast sends TEARDOWN alone). The relay then goes away, which is the hard stop.

**Events (`events.rs`)**
- The body is a bplist `{params: {data: <bplist>}}`. The inner plist is the event.
- `{type: "playbackState", name: …}` (or `params.playbackState`, robkochman's form): `loading` | `playing` | `paused` | `stopped`.
- `{type: "notification", name: …}`: `currentItemChanged`, `timeJumped`, `playbackLikelyToKeepUp` (→ not buffering), `loadedTimeRangesChanged`, `playbackBufferFull`, `itemPlayedToEnd` (→ `Ended`).
- `updateInfo` is acked and ignored.
- `{kind: "response", messageID}` answers a `playbackInfo` request.
- Unknown types are acked and ignored, logged once at debug.
- **Ready** is the first `playing`. The 30 s load timeout matches foxCast's.
- **End of media** (foxCast `waitQueue`) is any of:
  - `itemPlayedToEnd`;
  - `stopped` after `playing`;
  - the TV closing the connection after playing. tvOS does this when the user stops from the remote, and sends no `stopped`.
  
  Without `itemPlayedToEnd`, the end is `Lost(StoppedOnTv)`.

**Position (`position.rs`).** No reference reads a position from `playbackState`, so that is not relied on [REF].
1. **A `playbackInfo` request.** It is sent every 10 s while VOD or catch-up plays, and once before Play here, Stop and every reload of the item under the same generation. It is `/command {type: "playbackInfo", kind: "request", messageID: n}`, answered on the event channel. Source: PR 2846.
   - **The answer as tvOS 27 sends it** (AppleTV14,1, 24J361, traced 2026-10-13): `{kind: "response", messageID: n, type: "playbackInfo", info: {position, duration, rate, playbackState, loadedTimeRanges, …}}`, every time a CMTime dict `{value, timescale, flags, epoch}`. A growing playlist's `duration` has `flags 17` (valid + indefinite): no duration. LIVI's form (`value` with plain numbers) is read too.
   - Revision 2 read only `value`: the TV's answer, which came 40 ms after the request, was taken for no answer, and every session estimated.
2. **An estimate.** If the first request gets no answer within 3 s, or a non-2xx, the session switches to an estimate for the rest of the session:
   - it starts where the TV says its playhead is: `timeJumped` carries the position (a CMTime) at every start and after every seek, also one made with the TV's remote;
   - after a jump the TV doesn't place (a `timeJumped` without a position), or any break in the run of segment fetches (AVPlayer fetches ahead in order, so a fetch that doesn't start where the last ended is the playhead moving there), it continues from the start of that first fetched segment: behind the playhead, never ahead;
   - it advances while `playing`, and freezes on `paused` and `loading`;
   - it is bounded above by the end of the fetches since the last jump (VioletRelay bounds by fetches for receivers without metadata);
   - it is reported 2 s behind: it becomes the saved progress and a reload's start, which must never be ahead of what the viewer saw.
   
   It is accurate to about one segment. That is enough for the 10 s progress saves, a reload and Play here.
3. `GET /playback-info` is never polled: it answers 500 on 26.x and 27.0.

Positions are reported absolute. The session adds the item's offset: the `-ss` start, or the first segment of the playlist view. The TV starts at 0 of every playlist: EVENT playlists say `EXT-X-START:TIME-OFFSET=0` (§8.1.4), else AVPlayer starts a growing playlist three target durations from its end (it did: 12.18 s into a film's first playlist, 9.22 s into its reload's).

**Reloads continue where the TV is.** A reload under the same generation (an escalation, Convert this channel, a segment that outgrew the target, a stereo retry) starts at the position asked now (`playbackInfo`, else the estimate), never at the last 10 s report, nor at the cast's first start when this generation hasn't reported yet. A reconnect by the controller (a new generation) starts where its TV clock says the TV is (§11.2), 2 s back as ever.

- **The session outlives zaps.**
- **No metadata is sent in Phase 1.** If a title is ever sent to the TV, it goes through the spoiler guard and the parental view first.
- **Trace (dev only, `trace.rs`).** With `TUNER_CAST_TRACE=1`, a session writes a redacted JSON transcript to `state_dir/cast-trace/` (0600). It holds request lines, statuses, decoded plists and event bodies. Tokens, the LAN URL path, addresses, MACs and ids are replaced by placeholders. §18 uses it once to turn the real TV's replies into fixtures.
  - **Bounded.** A transcript stops at 10 MiB (`trace::MAX_FILE`; a VOD session wrote 1.7 MB an hour on 2026-10-13) with one last line, `{"t", "stopped": "size cap", "cap"}`; the session goes on untraced. The directory keeps the newest five (`service::TRACE_KEEP`).
- **Field logs.** One INFO line when a session ends and one when an item ends, each with why: the player closed it (the controller's own line says Play here, Stop casting, stopped, stopped on the TV, moved to another TV or shutdown), another cast took over, shutdown; replaced by the next item, stopped, played to its end, failed (the kind), lost or stopped on the TV. Test on TV logs its start and verdict.

### 7.4 What the AirPlay client can do

| Capability | Phase 1 | Evidence |
|---|---|---|
| Play / pause | `setRate 1.0 / 0.0` | VioletRelay (stop/continue), foxCast `Rate` [REF] |
| Seek | Replace with a playlist view inside what's produced, or a `-ss` restart beyond it (§11.3) | foxCast: "no source documents a play-queue seek". VioletRelay seeks by replace [HW 26.5]. The iOS `seek` request (LIVI) is unconfirmed on tvOS. |
| Zap / live reconnect | replace on the same session; on error, a full session restart | VioletRelay |
| Stop | `setRate 0`, TEARDOWN, close; then the relay goes away | VioletRelay, foxCast. A dead relay always stops the picture. |
| Volume | none | `volumeControlType 4` on the Apple TV. MRP-over-AirPlay is explored in Phase 4 (PR 2919). |
| End of VOD | `itemPlayedToEnd` | becomes `Ended` → Up next |
| Stopped on the TV | the connection closes or `stopped` arrives after `playing` | `Lost(StoppedOnTv)` (foxCast `waitQueue`) |
| Position | `playbackInfo` request, else an estimate | no polling (`/playback-info` returns 500 on 26.x and 27) |

### 7.5 Credentials at rest

- **Tables (migration `0014_cast.sql`):**
  - `cast_identity(id=1, pairing_id, seed BLOB, sender_mac)`: one long-term Ed25519 seed per install. `sender_mac` is a random, locally administered MAC used as the SETUP `deviceID` and as the fallback `channelID`.
  - `cast_device(id, protocol, name, model, os_version, source_version, last_addr, last_seen, peer_id, peer_ltpk BLOB, pair_kind, paired_at, last_used)`.
- **Protection.**
  - The database is 0600 in a 0700 directory (DESIGN §20, "At rest").
  - `backup.rs` exports an explicit table list (checked), so these tables never appear in a backup. A test asserts this.
  - Pairings are per machine. After a restore onto a new machine the user pairs again, the same way `[agents]` stays machine-local.
- **In memory.**
  - The seed is held as `Secret<[u8; 32]>`, zeroed after use, never `Debug`-printed and never sent to an agent.
  - Writes go through `db-writer`.
- **Config holds no secrets** (§20).

### 7.6 tvOS 26 and 27

The Apple TV auto-updates, and tvOS 27 shipped on 2026-09-14. Apple's only AirPlay note for 27 is "AirPlay performance improvements deliver faster, more responsive connections". The 26.x notes list no protocol changes [TV27].

**One code path.** Each difference below is handled by behaviour that is also correct on 26. Nothing in the protocol path branches on the version.

| # | Difference | 26.x | 27 | What Tuner does | Source | Fake mode that tests it |
|---|---|---|---|---|---|---|
| 1 | PIN UI needs a client name | not needed | **required** (`clientNameIsNil=YES … Bailing`) | always sends `X-Apple-Client-Name` on pin-start and every pair-setup | pyatv #2866, PR 2894; 14,1 confirmed | V27 shows no PIN without it |
| 2 | HKP 5 + ScreenCapture ACL | — | used by senders that work on 27 | the fallback variant (§7.2) | foxCast, doubletake | `hkp5_only` |
| 3 | Allow Access "Everyone in Your Home" → empty 200 on pair-verify | — | 27.2, one user | `PairingRefused` + the setting hint | pyatv #2885 | `allow_access: HomeOnly` |
| 4 | Allow Access reset by a major update | — | possible | `PairingLost` copy names the setting | postlund (pyatv) | — |
| 5 | Extra TLV type 17 (159 B) in M6 | seen on 26.6 | — | unknown TLVs ignored, signature still verified | send-airplay2 PR 8 | `m6_tlv17` |
| 6 | Event `/command` split across frames (1024 + 1259 B) | 14,1 on 26.6 | — | reassembled to Content-Length, acked once | castcli #35 | `split_events` |
| 7 | Inbound frames over 1024 B | — | claimed, unverified | accepted up to 65 535 B | pyatv decrypt, doubletake reader | V27 sends an 8931-B frame |
| 8 | `skipRecord` in SETUP | — | mirroring only; unverified for video | honoured; RECORD errors non-fatal | foxCast `mirror.go`, doubletake | `skip_record`, `record_status` |
| 9 | NTP timing collapses | 26.5 | 27 | declared PTP only | PR 2899, foxCast docs/05 | `strict_ntp` (Tuner must never declare NTP) |
| 10 | `/play`, `/playback-info` dead | 26.x | 27.0 | never used | pyatv #2906, castcli #35, rust-atv | the fake answers 500 |
| 11 | A multivariant playlist with an 80 Mbit `BANDWIDTH` and no `FRAME-RATE` never played | — | 27 | Tuner serves media playlists only (§8.4) | foxCast docs/05 | relay test |
| 12 | `X-Apple-Client-Name` reported optional again in 27.0 beta 5 | — | unclear | sent anyway | HA #173352 | — |

**Detection.** These are read for logs, `diagnose` and Settings only:
- TXT `osvers` (e.g. `26.6`) and `srcvers` (`960.13.1` on 26.6; `1005.12.1` reported on 27.2);
- `/info` `osBuildVersion` (`23L773` on 26.6) and `sourceVersion`.

Each session logs one INFO line without addresses: `cast: AppleTV14,1 tvOS 26.6 (23L773) AirPlay 960.13.1 pairing=hkp3 timing=ptp`. The registry updates the stored version, so Settings ▸ Cast shows the TV's current tvOS.

**Testing.** Every scenario in `tests/cast_airplay.rs` runs against the fake in both `Tvos::V26` and `Tvos::V27` modes, and the V27-only negatives run on top (§14.3). After the TV updates, the user repeats the pairing, session and controls items of §18.

**Not relied on.** A comment claims that 27.5 fixed AirPlay PIN pairing to home theatres. It is unverifiable and concerns receivers, so it is ignored [TV27].

### 7.7 Third-party AirPlay TVs (the Hisense)

- The profile is chosen when the `model` doesn't start with `AppleTV` (VioletRelay's detection) and features bit 33 is set.
- The differences come from VioletRelay `airplay_modern.py` L84–300. VioletRelay reports them working on a Hisense 100E75LE, the same family as the user's 85E75LE:
  - a playback-stream `SETUP {streams:[{type: 120}]}` before the type-130 SETUP;
  - `SETPEERSX /peer-list-changed-x` is sent, and a 404 or 501 is tolerated;
  - `/command` goes out as `POST /command RTSP/1.0`, as VioletRelay sends it through pyatv's `RtspSession.exchange` (with CSeq, DACP-ID, Active-Remote and Client-Instance);
  - the stream-id header on `/command` is negotiated in this order: `X-Apple-Stream-ID`, none, `X-Apple-StreamID`. `X-Apple-ProtocolVersion` and `X-Apple-Session-ID` are never sent on `/command`: the Hisense SDK rejects them on the queue endpoint;
  - the type-120 and type-130 SETUPs carry VioletRelay's stream headers: `X-Apple-ProtocolVersion: 1`, `X-Apple-Session-ID: <the base SETUP's sessionUUID, lower case>` and `X-Apple-Stream-ID: 1`;
  - the item adds `HLS-Content-Location` (the same URL), `isAudioOnly false`, `streamType 1`, and a `Start-Position` with timescale 1000 and value 0. VioletRelay: "the receiver uses this key—not the URL suffix—to select its HLS/MSE player".
  - the identifiers come from the TXT record, as VioletRelay reads them (`airplay_modern.py` L281–283, L331, L407): the `channelID` is `<TXT psi, else TXT pi>-RCS-1`, and `SETPEERSX`'s `ClockPorts` is keyed by the TXT `gid`, else that id. The Hisense's `/info` has only `pi` (no `psi` or `gid`). Discovery keeps the TXT identifiers with the device; only a TV the record never named (one added by address) falls back to `/info`'s, then to the sender MAC.
- Its player is hls.js 4.0.16 (`playbackCapabilities`), so HLS only: L2 TS for progressive H.264, L3 otherwise.
- §18 item 14 runs it against the 85E75LE. If it fails there, the profile is fixed, or third-party TVs are no longer listed (no hidden tier) until Phase 2/3 reach them over Cast or DLNA.

---

## 8. Media pipeline (`src/cast/relay/`)

### 8.1 Components

```
provider ──(reqwest: Playable.headers, A's lease, ReconnectPolicy)──▶ upstream proxy 127.0.0.1:P
            ▲ exactly one upstream request at a time per token       │  /u/<tok>          (head buffer replay)
            │                                                        │  /u/<tok>/r/<n>    (rewritten HLS URIs)
            │                                     ffprobe (once) ────┤
            │                                     ffmpeg ────────────┘──▶ session dir (tmpfs live / cache VOD)
            │                                                                │ index.m3u8, s<n>.ts|.m4s, init.mp4
            │                                     playlist guard + views (TARGETDURATION, IDR check, start segment)
TV ◀──────── LAN server <lan-ip>:47800 /c/<tok>/… (token, peer check, no redirects) ◀───────┘
```

**1. Upstream proxy (`upstream.rs`), loopback only.**
- **Ownership.** It is the only code that talks to the provider. It owns reconnects, so ffmpeg runs with `-reconnect 0` [PL §2].
- **What it reports to the controller.** Real HTTP statuses (`RemoteEvent::Http`) and provider text bodies (`ProviderText`, redacted). That keeps the §11.4 policy working: 403 race window, sticky live, the provider-message trap.
- **Head buffer for live.**
  - The first ≤ 16 MiB is kept and replayed from byte 0 to the next reader.
  - So ffprobe and then ffmpeg read the same first bytes without opening a second provider connection [PL §4].
  - The buffer is dropped once ffmpeg passes it. If ffmpeg doesn't attach within 10 s, the attempt is restarted.
- **One upstream request per token at a time.** A new request (ffmpeg seeking with `Range` on VOD) cancels the previous one.
  - Every upstream request carries `Range: bytes=<from>-`, from 0 too, as ffmpeg's `http` (and so mpv) sends it. The proxy tells ffmpeg the input is seekable (206, `Content-Range`) only when the provider is (a 206 or `Accept-Ranges: bytes`); a provider that ignores `Range` is served as a plain 200, so ffmpeg never seeks to bytes the proxy could only reach by buffering everything before them.
  - Each provider read is timed on its own (15 s, the player's `network-timeout`), never the gap since the last one: a pump held back by its reader (a paused TV with ffmpeg stopped at the VOD cap) isn't a stalled provider.
  - **A film's stream that breaks mid-body resumes** where it broke: a sized resource the provider serves by `Range` is asked again from the byte the reader reached, inside the same response to ffmpeg, up to three times per feed, each after 1 MiB of progress. Live streams and HLS segments fail as before. On 2026-10-13 the provider closed a film's connection 30 s into a cast; that had cost a reconnect and a reload on the TV.
  - **Why a stream broke is logged as its cause.** reqwest's `Response::chunk` files every failure to read a body as "error decoding response body" (0.13.5, `async_impl/response.rs`), a provider hanging up too; the client sets no read timeout (reads are timed one by one, as above, and our own timeout says "nothing for 15 s"), so the 2026-10-13 break was the provider's side, not backpressure. The log now gives hyper's cause: "error reading a body from connection: end of file before message length reached" (the provider hung up with bytes owed), "…: connection reset by peer".
- **HLS upstream: the proxy picks one variant itself.** It rewrites the URIs to `/u/<tok>/r/<n>` (a table of `Secret` upstream URLs) and serves ffmpeg a single media playlist.
  - The pick is the variant mpv was playing, else the highest bandwidth the receiver can take.
  - Otherwise ffmpeg's HLS demuxer may open several variants at once and break a one-connection account.

**2. Probe (`probe.rs`)**
- Runs `ffprobe -v error -of json -show_streams -show_format -probesize 3M -analyzeduration 3M http://127.0.0.1:P/u/<tok>`.
- The JSON gives codec, profile, level, `field_order`, `pix_fmt`, size, frame rate, audio codec and channels.
- **Skipped** when the hints are already enough: the moving item's mpv hints (§5.5), or a cached `cast_hint` for this item and receiver less than 30 days old. Never for multichannel sound on the Apple TV: the surround rendition needs the layout, profile, language, frame rate and transfer only the probe gives.
- A failure with a cached hint re-probes once.

**3. ffmpeg (`ffmpeg.rs`)**
- Spawned through `tokio::process` with `kill_on_drop` and `PR_SET_PDEATHSIG`.
- Its environment is cleared except `LC_ALL=C`.
- Its argv holds only the loopback URL and the output directory.
- stderr is redacted and logged at debug, capped.
- `-progress pipe:1` gives `speed=`. A live speed below 0.98× for 20 s gives `CpuTooSlow`.
- Capabilities are checked once per run (`ffmpeg -hide_banner -encoders` for `libx264` and `aac`). If either is missing, L3 is off and the UI says so.

**4. Playlist guard (`playlist.rs`).** Tuner serves ffmpeg's playlist through a small rewriter. It addresses two unverified risks [PL §1]:
- **Stable target duration.** `#EXT-X-TARGETDURATION` is pinned when the first segments are ready and never changes while a playlist is served (RFC 8216 §6.2.1; in copy mode ffmpeg derives it from its window and changes it).
  - **The pin:** `ceil(max(hls_time, probed max GOP))`, at least the longest first segment (rounded), and for copied video 2 s above the longest of them all. Copied video cuts at the source's keyframes: segments run up to `hls_time` plus a GOP, and a film's scene cuts make its GOPs anything up to the encoder's maximum. Copied VOD pins on 20 s of its first burst (or 3 s more), not on two segments.
  - **One per item.** The relay keeps the pin for the item for the whole session: every later load of it (a reconnect, an escalation, a restart) declares at least as much. On 2026-10-13 one film's loads declared 10, 8 and 4 s.
  - **A longer segment** (its rounded `EXTINF` above the target) is held back with everything after it, and the session restarts the item where the TV is, at the same level, under a pin that covers it (its length + 2 s): a replace with a new token, a new playlist, no changed TARGETDURATION. A segment beyond 30 s is a violation: keyframes that sparse need converting.
  - **Revision 2 raised the target mid-stream** and counted each raise as a violation: the 2026-10-13 film (HEVC, segments of 10.09 s and 12.10 s) was "irregular" after two and went to L3, at 700 % CPU on the M2, and restarted. A long GOP is no irregular source.
- **IDR check.** The first video access unit of each new segment is checked for an IDR (H.264 NAL type 5, HEVC IRAP 16–21). Apple rule 7.4 requires it, and ffmpeg copy mode may cut on recovery-point I-frames.
- **Escalation.** Two violations (missing IDRs, or a segment past any target) in a session escalate an L2 generation and are remembered as an "irregular" L2 failure in `cast_hint` (§8.3), so its next start uses L3, where forced keyframes fix them.
- **Start.** EVENT playlists say `#EXT-X-START:TIME-OFFSET=0`, so the TV starts where the relay started (§7.3).
- The guard never adds absolute URLs; ffmpeg writes relative segment names.

**5. Playlist views (`playlist.rs`).** A served playlist can start at segment *k* of what ffmpeg has produced:
- it has `#EXT-X-MEDIA-SEQUENCE:k`, the segments from *k* on with their `PROGRAM-DATE-TIME`, and the same type (EVENT for VOD and catch-up);
- it is served under the new generation's token, while the segment files are shared on disk;
- a seek inside what's produced becomes a replace (§7.3) with such a view. The TV starts at 0 of it, and the session adds segment *k*'s start time to the positions it reports;
- the playlist only ever grows at the end, as RFC 8216 requires for EVENT playlists.

### 8.2 Hints, in priority order

1. The receiver profile (Apple TV / third-party AirPlay TV; Chromecast models in Phase 2).
2. The user's per-item override ("Convert this channel" → L3).
3. The cached `cast_hint(item, receiver)`: an L2 failure remembered under today's logic (→ L3), interlaced, codecs.
4. mpv's hints when moving the item that's playing.
5. ffprobe on the head buffer (or with Range on VOD).

### 8.3 Decision tree and fallback ladder (`plan.rs`, pure, table-tested)

```
plan(receiver, kind, hints, cfg) →
  DRM / encrypted ............................................ Refuse(Unsupported)
  user override or cast_hint says L3 ........................ L3
  video not in receiver profile (MPEG-2, VC-1, VP9 on Apple…)  L3 (if cfg.convert = auto, else Refuse(Unsupported))
  H.264 above receiver level (Cast gen 1/2, Phase 2) ......... L3
  interlaced && cfg.interlaced = auto && profile.deinterlace . L3 + yadif   (AirPlay: Apple HLS rule 1.14)
  HEVC ...................................................... L2 with fMP4 segments (Refuse→L3 where HEVC unsupported)
  otherwise ................................................. L2 (TS segments)
  audio: AAC ≤ 2 ch → copy; everything else → AAC stereo 160k (Apple rule 2.3)
  surround: Apple TV, a picture, > 2 ch ...................... + a surround rendition (§8.4), every rendition fMP4:
      AC-3, E-AC-3, AAC ≤ 5.1 → copy; DTS, TrueHD, MP2/PCM 5.1, AAC 7.1… → AC-3 5.1 448k
  L1 (byte relay) ........................................... Phase 2: faststart MP4 VOD; DLNA live TS (Phase 3)
```

- **Interlaced input on AirPlay is always deinterlaced.** Apple's HLS Authoring Specification rule 1.14 says "All interlaced source content MUST be deinterlaced." This is what Phase 0 was going to measure.
  - `yadif=deint=interlaced` keeps the frame rate (25p from 25i). Rule 1.15 recommends double rate as a SHOULD; that doubles the CPU-only encode cost, so it is not taken in Phase 1.
  - "Send as is" (`interlaced = "as-is"`) is the user's opt-out when the CPU can't keep up. L2 copy is then used, and "Convert this channel" still works.

**Levels and their settings**

| Level | ffmpeg | Shown as |
|---|---|---|
| L1 | none (Range relay, `Content-Length`, HEAD) | "Original" |
| L2 | `-map 0:v:0 -map 0:a:<chosen> -c:v copy -c:a copy │ aac -ac 2 -b:a 160k` | "Repackaged" |
| L3 | `-c:v libx264 -preset veryfast -profile:v high -level 4.1 -crf 21 -maxrate 8M -bufsize 16M -g <2×fps> -sc_threshold 0 [-vf yadif=deint=interlaced,scale=-2:'min(1080,ih)']` + AAC stereo | "Converted" (CPU only: this M2 has no usable hardware encoder [PL change 3]) |

**Escalation at runtime.** One step only, then the result is cached in `cast_hint`: when the step is taken, not at L3's first picture, because L2's 15 s and L3's own start can outlast the controller's 30 s load timeout on live, and its retry (a new generation) must start at L3 rather than on L2 again. It continues where the TV is (§7.3). Any of these moves L2 to L3:
- the TV fetched segments but never reached `playing` within 15 s;
- the TV reported a failure;
- the playlist guard tripped (missing IDRs, a segment past any target).

**What is remembered** (`cast_hint`, `store::cast`). The step records an *L2 failure* for the item on that TV: when, whether the guard found it (`irregular`), and the version of the relay logic that judged it (`L2_FAILURE_LOGIC`). Verdicts are versioned because the logic changes: on 2026-10-13 the guard of the morning called a long-GOP HEVC film irregular, and the row it left sent every later cast of the film straight to L3 under the fixed guard. So a failure forces L3 only when today's logic judged it, for 30 days counted from the failure (L3 pictures don't extend it), and the first L2 picture of the item on that TV clears it, as does **Send as is** (§3.3). A cached `level` never forces L3 by itself: a format that needs converting is re-decided from its codecs. Bump `L2_FAILURE_LOGIC` with any change to what counts as a failure.

A segment that outgrows the pinned target is no reason to escalate: it restarts the item at the same level (§8.1.4). A surround item the TV fails (either of the first two) goes again in stereo first, at the same level and place, without spending the step; ffmpeg failing to make the renditions does the same inside the relay.

The audio track is the one mpv had selected, else the default track, else the first. Subtitles and teletext are dropped in Phase 1.

### 8.4 HLS parameters

**Live**

```
-f hls -hls_time <max(4, probed max GOP)> -hls_list_size 20 -hls_delete_threshold 4
-hls_flags delete_segments+independent_segments+program_date_time+temp_file+omit_endlist
-hls_segment_type mpegts            # H.264
-hls_segment_type fmp4 -tag:v hvc1  # HEVC (Apple 1.5; never HEVC in TS)
-hls_segment_filename s%d.ts|.m4s   output: $XDG_RUNTIME_DIR/tuner/cast/<session>/ (0700, tmpfs)
```

- **Required flags:**
  - `program_date_time`: Apple rule 8.4 says it MUST be present.
  - `temp_file`: a segment is never served half-written.
  - At least 6 segments in the window (rule 8.11). 20 × 4 s gives 80 s or more, enough to pause and resume, at about 80 MB of tmpfs at 8 Mbit/s.
- **Segment length.** `hls_time` is 4 s, VioletRelay's value, which works on its tvOS 26.5 hardware [REF]. That is short of the 6 s Apple rule 7.5 recommends (a SHOULD, not a MUST), trading the recommendation for tune-in time.
- **When the TV is told to play.** `insertPlayQueueItem` is sent once the playlist holds `cast.min_segments` segments, default 2. This is VioletRelay's `wait_until_ready(segments=2)`, or ENDLIST for a short item.
- **Expected delay behind live:** about 3 × TARGETDURATION plus one segment fill, which is roughly 10–20 s depending on the channel's GOP [PL §1].
- **Media playlists, except for surround.** Stereo items give the TV one media playlist. Surround items get a multivariant playlist (below) that writes the measured `BANDWIDTH` and `AVERAGE-BANDWIDTH`, `FRAME-RATE` and `CODECS`. On tvOS 27 a variant with an 80 Mbit `BANDWIDTH` and no `FRAME-RATE` was fetched but never played (foxCast) [REF].
- **The live hold-back grows with the pin's headroom:** about 3 × TARGETDURATION, so a 4 s channel whose first window was 4–6 s runs 18–24 s behind (12–18 s before). That is the price of never restarting a live channel for a segment a little over its target.

**Surround (multichannel sound on the Apple TV)**

Verified against Apple's *HLS Authoring Specification for Apple Devices* (revision of 2025-06-26) and Apple's bipbop advanced example:
- 2.3: stereo AAC MUST be provided; 2.5: the multichannel formats include AAC, AC-3 (`ac-3`) and E-AC-3 (`ec-3`); 2.6: E-AC-3 needs an AC-3 companion only for devices without it (the Apple TV 4K decodes both, "Audio compatibility"); 9.6: multichannel audio MUST be a separate audio stream, declared with `EXT-X-MEDIA` (8.9) with `LANGUAGE` (8.10); 8.2: every playlist of a presentation has the same target duration; 9.20 (tvOS): no audio-only variant.
- The appendix "Audio rendition groups and variants": one group per codec and channel count, a stereo AAC group as the fallback every device plays, each video variant listed once per group (bipbop: `aud1` AAC, `aud2` AC-3 `CHANNELS="6"`, `aud3` E-AC-3, the whole ladder repeated per group). AVPlayer picks the highest bandwidth it can play on its audio route; the Apple TV decodes Dolby and multichannel AAC and sends multichannel PCM by default (Apple's tvOS audio settings), so a stereo TV gets the stereo group.

So a source with more than two channels (and a picture) on the Apple TV gets:

```
ffmpeg … -map 0:v:0 -map 0:a:<n> -map 0:a:<n> -c:v copy [-tag:v hvc1]
         -c:a:0 aac -ac:a:0 2 -b:a:0 160k
         -c:a:1 copy [-bsf:a:1 aac_adtstoasc] | -c:a:1 ac3 -ac:a:1 6 -b:a:1 448k
         -f hls -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4
         -var_stream_map "v:0,name:v a:0,name:a a:1,name:s"  -hls_segment_filename <dir>/%v/s%d.m4s <dir>/%v/index.m3u8

index.m3u8 (written by the relay; the renditions are ffmpeg's, served under the same token):
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="surround",LANGUAGE="en",NAME="Main",AUTOSELECT=YES,DEFAULT=YES,CHANNELS="6",URI="s/index.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="stereo",LANGUAGE="en",NAME="Main",AUTOSELECT=YES,DEFAULT=YES,CHANNELS="2",URI="a/index.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=…,AVERAGE-BANDWIDTH=…,CODECS="hvc1.2.4.L150.B0,ac-3",RESOLUTION=…,FRAME-RATE=…,VIDEO-RANGE=SDR,AUDIO="surround"
v/index.m3u8
#EXT-X-STREAM-INF:…,CODECS="hvc1.2.4.L150.B0,mp4a.40.2",…,AUDIO="stereo"
v/index.m3u8
```
- The video codec string and size come from the video rendition's init segment (`avcC`/`hvcC`), the bandwidths from the first ready segments, the frame rate and `VIDEO-RANGE` (SDR, PQ, HLG) from the probe, `LANGUAGE` from the stream's tag (`eng` → `en`, else `und`). Copied E-AC-3 with Atmos says `CHANNELS="16/JOC"`.
- Copied: AC-3, E-AC-3 and AAC up to 5.1 (no re-encode). Converted: anything else to AC-3 5.1 at 448 kbit/s (7.1 folded to 5.1, AC-3's most), which every Dolby receiver decodes and which needs no companion.
- Every rendition is fMP4 (rule 2.1 wants audio as elementary streams or fMP4; one muxer writes all three), H.264 included (rule 1.2 allows it). A stereo item keeps TS for H.264, as before.
- **One timeline.** Alternate renditions must carry the video's timestamps, and the Apple TV aligns them on the fMP4 `tfdt`, not on edit lists. ffmpeg's fMP4 muxer (one per rendition) starts every track's `tfdt` at 0 and says where it really starts only in an empty edit; and with `-ss` copied streams start at the demuxer's seek position (up to a GOP early) while converted ones start at the seek point. On 2026-10-13 a resumed, converted surround cast played its copied 5.1 10 s late; measured with a marker film, resumed copied films were 3.4–10 s off in stereo too, and every fMP4 output 59 ms off from the start. So fMP4 is written with `movflags=+frag_discont+negative_cts_offsets` (real decode times in `tfdt`, the first picture's presentation time its decode time: no edit list), copied video starts every stream at its keyframe (`-noaccurate_seek`) and converted video drops copied sound before the seek point (`-copypriorss:a 0`). Every mode then measures within 21 ms (`relay::ffmpeg` module docs; `tests/cast_relay.rs` measures it). MPEG-TS was never affected (one PTS clock).
- **Where a copied resume starts.** Copied video resumed at X starts at the keyframe K ≤ X (up to a GOP early), and the output's timestamps don't say where K was (the muxers shift the first packet to 0, MPEG-TS to 1.4 s). Positions and saved progress must never run ahead of the picture, so such a run copies its first video packet to a second output, a `framecrc` line in the pipeline directory (`start.crc`, flushed at once, never served) whose time is K − X on ffmpeg's shifted timeline; the relay starts the item at K, and the session adds K, not X, to the TV's positions. Without the line it counts from a whole first segment before X (a copied segment holds at least the GOP the seek fell into): early, never late.
- The audio renditions are cut every `hls_time`; copied video at its keyframes, so their boundaries differ (rule 8.22 is a SHOULD for Apple devices; AVPlayer aligns renditions on their timestamps). Third-party AirPlay TVs, for which Apple makes 8.22 a MUST, stay stereo.
- A view (a seek inside what's produced) starts each audio rendition at its segment holding the video's first.
- **Not verified on the TV yet** (§18 item 5): that tvOS 27 picks the surround group, and plays the renditions over AirPlay's URL playback. If it doesn't play them, the session's stereo retry keeps the cast going.

**Catch-up and VOD**
- They use `-hls_playlist_type event` (rule 8.8), with no `delete_segments` and with `ENDLIST` at the end.
- Output goes to `paths.cache_dir/cast/<session>/` on disk, because a film doesn't fit in tmpfs. It is capped at 8 GiB, wiped on stop and swept at startup.
- **Production stays about a minute ahead of the TV.** ffmpeg reads the first 60 s as fast as the provider sends them (a quick start, room for the TV's first buffer), then at real time with its 5 % catch-up (`-readrate 1 -readrate_initial_burst 60`). Revision 2 read at 4× after a 30 s burst: the film ran minutes ahead and the TV's scrubber showed it all as loaded.
- **The lead cap.** Production pauses (SIGSTOP, as at the 8 GiB cap) while it is more than 180 s ahead of the end of what the TV fetched (a TV paused for minutes), and resumes under 120 s. No film is pulled from the provider faster than it is watched.

### 8.5 Per content type

**Live**
- Uses L2 or L3 with a sliding window.
- **Zap.** The controller's normal zap leads to `RemoteCmd::Load`. The session then:
  1. stops the old pipeline and waits up to 1.5 s for the upstream close (stop-before-start);
  2. mints a new token;
  3. starts a new pipeline;
  4. replaces the item on the same AirPlay session (§7.3).

  Expect 6–12 s per zap. `Zapping` coalesces rapid presses, so only the final channel tunes.
- **Live reconnect.** Same as a zap, with the same channel.
- **Phase 2 experiment:** one continuous playlist across zaps and reconnects (`append_list` plus `#EXT-X-DISCONTINUITY`), so the TV doesn't reinitialise.

**Catch-up**
- Uses an L2 EVENT playlist, with `duration_hint` as the duration.
- A seek inside what's already produced is a replace with a playlist view starting at the target's segment (§8.1). It costs no new upstream.
- A seek beyond that clamps to the produced edge, with the banner "Catch-up can only seek as far as it has loaded". Phase 2 re-resolves `MediaRef::Catchup` with a later start through the existing timeshift builder.

**VOD (film or episode)**
- Uses an L2 EVENT playlist.
- **Resume, or seek beyond the produced edge**, restarts ffmpeg with `-ss <t>`. That turns into a `Range` request through the proxy, so there is still one upstream connection. The new playlist starts at `t`, so the TV starts at its 0.
- The session adds `t` (or the view's first segment time) to the positions the TV reports, so the snapshot, progress and `finished` stay true to the film.
- A seek inside the produced part is a replace with a playlist view.
- **The TV's own scrubber** shows what is produced: the position plus about a minute (§8.4), growing as it plays. Seeking with the TV's remote works inside that; Tuner's seek bar (and the agent) reach the whole film.

**Why not a complete VOD playlist (the film's real length on the TV, every segment listed, made on demand)?** That is how Jellyfin serves Apple TVs (facts: its `DynamicHlsController` lists every segment up front and restarts ffmpeg at the segment the client asks for). It needs the segment boundaries before anything is produced, and that holds only where the relay chooses them:
- **Copied video (L2, almost every film)** can only cut at the source's keyframes, and ffmpeg's HLS muxer decides where (the first keyframe past each `hls_time` step, counted from where that ffmpeg started), so a restart at segment k cuts differently from a run that reached k. A precomputed list needs every keyframe time (an MP4's `stss`; an MKV's `Cues` may list only some), `hls_time` below the shortest GOP so ffmpeg cuts at each one (segments as short as a scene cut), restarts on exact keyframes with `-copyts` (ffmpeg drops copied packets before `-ss`), and audio renditions cut at those same times (the muxer cuts audio-only renditions every `hls_time`, so surround would break it). MPEG-TS films and catch-up have no index at all, and catch-up can't restart at an arbitrary point (§8.5 "Catch-up").
- **Converted video (L3)** has fixed 4 s segments, so it could; it is the rare path (MPEG-2, VC-1, VP9 films, or Convert this channel), runs near real time on this M2, and a scrub would restart the encoder anyway.

So Phase 1 keeps the EVENT playlist with the lead and start fixes above. The complete playlist is the Phase 2 item "Full-length VOD on the TV" (§15.3): MP4 `stss` and MKV `Cues` read through the proxy by `Range`, one segment per keyframe run, on-demand restarts, L3 first.

**Test on TV**
- The input is `-f lavfi -i testsrc2=s=1280x720:r=25 -f lavfi -i sine=f=440 -t 15`, plus a drawn "Tuner" label, through L3 into the same server.
- It makes no provider connection and doesn't involve the controller. It runs as `CastHandle::test_on_tv(device)`.

---

## 9. Local server and security (`relay/server.rs`, `relay/token.rs`, `firewall/`)

### 9.1 Server

**Stack**
- `axum` 0.8.9 is promoted from dev-dependency to a normal one. It is already compiled for tests, so the cost is near zero.
- It is built with `default-features = false` and the features `http1` and `tokio`, and served with `into_make_service_with_connect_info::<SocketAddr>()`.
- There is a **single fallback handler**: the path is parsed by hand, so every miss gets the same 404.

**Bind**
- Only the address the kernel would use to reach the receiver, found with the UDP `connect()` trick. [NE] confirmed 192.168.1.88 on wlan0, which matches the Apple TV's echoed `senderAddress`.
- **Never `0.0.0.0`**, and IPv4 only in Phase 1.
- On a fixed, configurable port: `cast.media_port`, default **47800/tcp**. A random port can't be opened in a firewall.
- **It listens only while a session exists**, plus during Test on TV.

**Requests**
- **Routes:** only `GET` or `HEAD` on `/c/<tok>/<file>`, where `<file>` matches `^(index\.m3u8|init\.mp4|s\d{1,7}\.(ts|m4s))$`. Every other path or method returns the same 404 (not 405), with no body differences.
- **Peer check:** the peer IP must be the session receiver's IP. A connection from any other host is closed as soon as it is accepted, before it takes one of the 8 connection slots or is read: the ufw rule may admit the whole subnet, and a host holding every slot would stall the TV's fetches. The handler checks the peer again and answers a mismatch with the same 404, so the server can't be used as an oracle. These misses are logged at debug, redacted.
- **No redirects ever** (Apple rule 8.18).

**Responses**
- Correct MIME types: `application/vnd.apple.mpegurl`, `video/mp2t`, `video/mp4`.
- `Content-Length` always (segments are complete files thanks to `temp_file`).
- `Range` → 206 on files (AVPlayer starts with `bytes=0-1` [AP 1.3]).
- `Cache-Control: no-cache` on playlists.
- `Access-Control-Allow-Origin: *` plus `Range` in Allow-Headers, and OPTIONS answered from Phase 2. Cast MSE needs it, and it is harmless for AVPlayer; the token gates access.
- At most 8 concurrent connections per session, and idle connections close after 30 s.

**Phase 3 (DLNA):** `contentFeatures.dlna.org` / `transferMode.dlna.org: Streaming` on HEAD and GET; live TS with `DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000…` [DL §1].

### 9.2 Tokens

- 128 random bits from aws-lc-rs `SystemRandom`, base32 lowercase (26 characters), compared in constant time.
- **One token per load generation.** A zap, a seek (view or restart) or a reconnect revokes the old one. A playlist view's token maps to the same session directory.
- Held as `Secret<String>` and registered with `redact::register_secret` (`redact.rs:74`), because the LAN URL is a bearer credential to a live stream. Logs say `cast session <4-hex short id>`.
- Revoked when the session ends, and 60 s after the last fetch once playback has stopped.
- The loopback upstream proxy has its own token per session. It appears in ffmpeg's argv, so local users can see it in `ps`.
  - It only works from 127.0.0.1, while the session lives, one client at a time.
  - Omarchy is a single-user desktop. This is accepted and recorded in DESIGN §20.

### 9.3 Where a credential could leak, and what stops it

| Path | Guard |
|---|---|
| TV, its logs or its vendor analytics | The TV only ever sees `http://<lan-ip>:47800/c/<tok>/…`. There is no provider host, path or login. |
| ffmpeg or ffprobe argv (`/proc/*/cmdline`, world-readable) | Only the loopback token URL. Never the provider URL (DESIGN §20 "never on a command line"). A test asserts this. |
| Upstream HLS playlists | Rewritten to `/u/<tok>/r/<n>`. The real URLs stay `Secret`s in a table. |
| Served playlists | Relative names only. The guard never writes absolute URLs. |
| Logs (Tuner, ffmpeg stderr, receiver text) | `redact::redact` on every line. Tokens are registered secrets. No device IPs or LAN URLs at INFO. |
| The dev trace (`TUNER_CAST_TRACE`) | Tokens, URL paths, addresses, MACs and ids replaced by placeholders before writing; 0600; off by default. |
| `pkexec` argv (`ps`) | Only the interface, subnet and port, which are not secrets. No token, TV address or password. polkit's agent handles the password; Tuner never sees it. |
| Toasts, notifications and the casting card | Typed `CastErrorKind` copy. Never raw device or helper text. ufw's stderr tail is redacted and shown only in the firewall card. |
| Agent replies | Typed kinds. `guard::sanitize` turns any `://` into `internal`. Device names are sanitized. |
| Device-supplied strings (names, models, versions, plists) | Treated as data: capped and sanitized. Parsers are fuzz-capped (bplist depth and size). |
| Pairing keys | Kept in the 0600 DB, out of backups, `Secret`, zeroed. |
| PIN | Used once in memory. Never logged or stored. |

**Threat model (added to DESIGN §20).** This is Tuner's first LAN-facing listener and first privileged helper. The protections are:
- bearer tokens;
- a single route family;
- a narrow bind and a peer check;
- no way for a client to name an upstream;
- no directory listing;
- nothing listening when idle;
- a root helper that takes only validated, fixed-shape arguments and runs one fixed command (§9.4).

The residual risk is a LAN host that spoofs the TV's IP and also has the token. That is no worse than the TV itself.

### 9.4 Firewall (Omarchy ships ufw with `DEFAULT_INPUT_POLICY="DROP"` [NE A], [FW])

**What has to be open: TCP `cast.media_port` (47800), inbound, on the LAN interface, from the LAN subnet. Nothing else.**
- Everything else is outbound from Tuner and passes ufw (`DEFAULT_OUTPUT_POLICY="ACCEPT"`, `before.rules` accepts `RELATED,ESTABLISHED`). That covers RTSP to :7000, the event channel (Tuner connects to the TV's `eventPort`) and the provider.
- **No UDP.** Timing is declared PTP with no clock (§7.3), so there is no NTP port to open. Without a firewall, any PTP packets the TV might send to UDP 319/320 would meet closed ports; with ufw they are dropped. Either way Tuner takes no part, exactly as foxCast's URL path doesn't. §18 item 2 runs an hour with ufw on to confirm it.
- mDNS multicast is already allowed by `before.rules` (lines 68 and 72) [NE A].
- A tvOS 27 Home Assistant report needed "full access" to the Apple TV. That concerns traffic *to* the TV, which Tuner sends outbound [TV27].

**Detection, no root (`cast/firewall/mod.rs`, `ufw.rs`)**
- **Inputs**, all readable without root [FW §4]:
  - `/etc/ufw/ufw.conf` (`ENABLED`);
  - `/etc/default/ufw` (`DEFAULT_INPUT_POLICY`);
  - `/etc/ufw/user.rules`;
  - whether `/usr/bin/ufw` exists;
  - `systemctl is-active ufw`, `firewalld` and `nftables`.
- **Interface and subnet.** The UDP `connect()` trick gives the source IP towards the TV. `if-addrs` gives its interface and prefix, so (wlan0, 192.168.1.0/24) here.
- **The parser** reads `### tuple ###` lines. The format comes from ufw's own reader (`backend_iptables.py` L697–790) and the real lines on this machine:
  - the fields are action, proto, dport, dst, sport, src, then two app fields when present, then `dir[_iface]`, then `comment=<hex>`, which is stripped first (the app fields come before the direction in ufw's reader, so a 9-field line is `… src dapp sapp dir`);
  - actions `allow`, `deny`, `reject` and `limit`, each with `_log` or `_log-all`;
  - `route:` rules are ignored;
  - ports may be lists (`80,443`) or ranges (`8080:8090`);
  - proto may be `any`.
- **A covering allow** has all of:
  - action `allow*`;
  - proto `tcp` or `any`;
  - a port that equals the media port or includes it;
  - dst `0.0.0.0/0` or the LAN IP;
  - a src that contains the TV's IP;
  - direction `in` or `in_<iface>`;
  - and **no earlier** `deny`, `reject` or `limit` tuple matching the same traffic, because ufw is first-match.
- **Result** (`FirewallState`):

  | State | When | Settings shows |
  |---|---|---|
  | `NoFirewall` | ufw not installed, or `ENABLED=no`, or its unit inactive, and neither firewalld nor nftables active | "No firewall" |
  | `Allowed { ours }` | a covering allow exists. `ours` when it carries the `omatuner cast` tag | "Allowed" / "Allowed (Tuner's rule)" |
  | `Blocked { shadowed }` | ufw active, input policy DROP or REJECT, no covering allow. `shadowed` when an earlier deny matches | "Blocked (ufw)" / "Blocked by an earlier rule" |
  | `Unknown { why }` | firewalld active, nftables only, or the files unreadable | "Can't tell (firewalld)" etc. |

- It also returns `tagged`: every tuple whose comment decodes to `omatuner cast`. These are used for Remove and to spot rules for another interface or subnet.
- It runs on the runtime, because it spawns `systemctl`. The result is cached for 5 s per address it was made toward, and re-run on Check again, after Allow or Remove, and at each session start.
- **Toward which TV.** ufw rules are scoped by source (`from 192.168.1.0/24`, or one host), so a detection answers for the TV it was made toward and no other. Every `Detection` carries its `toward`, and `Detection::state_for(tv)` answers only when that is the TV's own address and a network toward it was found.
  - "The TV" for Settings, the firewall card and Allow (`CastApi::firewall()`) is the one asked about or cast to last, else the default TV, else any TV on the network.
  - With no TV known, detection goes toward the mDNS group (224.0.0.251) for the LAN's route. That address never sends the flow, so a LAN-scoped rule can't match it and the DROP policy would say `Blocked` for a port the TVs reach. Toward an address that isn't a host, a block is `Unknown { NoTv }` ("Can't tell until Tuner finds a TV"); a covering unscoped allow still counts.
  - Anything that refuses or holds a cast before a session judges by a fresh detection toward the TV's own address (`CastApi::firewall_for`): the agent's gate (§12) and the wizard's Network step. A detection toward another address, the mDNS group or no route never refuses. This was a real refusal (2026-10-13): an agent's cast to the Apple TV at 192.168.1.5 was refused with `cast-firewall` from a detection made toward 224.0.0.251 before any cast, while Tuner's own tagged rule allowed the TV.
- The files show what is configured, not what is loaded. A hand-flushed ruleset is invisible, so the end-to-end check stays the truth.

**End-to-end check**
- `insertPlayQueueItem` was answered 2xx, but the TV hasn't asked for `index.m3u8` within 10 s.
- The result is `Firewall` when the state is `Blocked` or `Unknown`, otherwise `ReceiverFailed`. [NE B.2] notes the Apple TV fetches within seconds.

**Remedy: the "Allow on firewall" button (`cast/firewall/allow.rs`)**

Tuner runs one privileged command through `pkexec`. polkit's agent asks for the user's password or fingerprint. On Omarchy that is the shell's themed dialog, on the focused monitor. Tuner never runs `sudo` and never sees the password.

*When the button shows*
- Shown when the state is `Blocked` (ufw), or a cast failed with `Firewall` while ufw is active.
- Hidden when ufw isn't installed, `/usr/bin/pkexec` is missing, or the state is `Unknown`. Those cases are copy-only.

*What runs.* The command is an argv with no shell, stdin is null and stderr is captured. It runs off the UI thread with a 120 s cap. Killing `pkexec` (Cancel, timeout, quit) closes the dialog.
- **Packaged (normal):**
  ```
  /usr/bin/pkexec --disable-internal-agent /usr/lib/tuner/tuner-firewall allow wlan0 192.168.1.0/24 47800 [--first]
  ```
  - `tuner-firewall` (`src/bin/tuner-firewall.rs`) is a std-only binary of about 150 lines. It is the only code that ever runs as root.
  - Its polkit action is `io.github.rthoms334.omatuner.firewall` (`packaging/polkit/…policy`):
    - `org.freedesktop.policykit.exec.path` is the helper;
    - `allow_active` is `auth_admin_keep`, so a Remove or port change soon after needs no second prompt;
    - the message is "Allow your TV to connect to Tuner for casting".
  - It validates everything before acting:
    - the verb is `allow` or `remove`;
    - the interface matches `^[A-Za-z0-9_.-]{1,15}$` and exists in `/sys/class/net`;
    - the CIDR is an RFC 1918 IPv4 network, /8 to /32, with zero host bits;
    - the port is 1024–65535;
    - it refuses unless the effective uid is 0 and `PKEXEC_UID` is set.
  - Then it runs exactly one fixed command:
    - `/usr/bin/ufw [prepend] allow in on <iface> from <cidr> to any port <port> proto tcp comment 'omatuner cast'`. `--first` gives `prepend`, used only when detection found a shadowing rule.
    - or `/usr/bin/ufw delete allow in on <iface> from <cidr> to any port <port> proto tcp`.
  - Exit codes: 0 done; 2 bad arguments; 3 ufw failed, with its stderr tail (≤ 2 KiB) passed through. It never uses 126 or 127, which stay pkexec's.
  - The pattern follows PearPlay's `helper/firewall.py` (MIT).
- **From a checkout** (the helper or its policy isn't installed): the generic `org.freedesktop.policykit.exec` action. Omarchy's dialog then reads "Authorize running '/usr/bin/ufw …'".
  ```
  /usr/bin/pkexec --disable-internal-agent /usr/bin/ufw allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment "omatuner cast"
  ```
- `--disable-internal-agent` is always passed. Without a graphical agent, pkexec would otherwise prompt on the terminal Tuner was started from.

*Outcomes* (`AllowOutcome`, from the exit status and then a re-read of `user.rules`; exit codes per `man pkexec`):

| Outcome | How it's detected | The card says |
|---|---|---|
| `Added` | exit 0, and the tagged rule is now in `user.rules` | "Allowed. Checking with Living Room…" then Check again (Test on TV in the wizard) |
| `AddedUnverified` | exit 0, rule not found on re-read | "Added, but Tuner can't see the rule. Checking with Living Room…" The 10 s check decides. |
| `Cancelled` | 126 (dialog dismissed), or the user's Cancel | nothing new; the command stays visible |
| `NoAgent` | 127, stderr mentions no authentication agent | "No password dialog is available. Run this in a terminal:" + the command |
| `NotAuthorized` | 127 otherwise | "Not allowed (needs an administrator). Run this in a terminal:" + the command |
| `Failed` | 1–125 | "The firewall refused: <ufw's message, redacted>" + the command; detection re-runs |
| `TimedOut` | 120 s | "Timed out." handled like Cancelled |

While polkit's dialog is up, the card shows "Waiting for your password…" with Cancel.

**Remove**
- Settings ▸ Cast ▸ Network shows **Remove rule** when a tagged rule exists.
- It runs `tuner-firewall remove` (or `ufw delete …` from a checkout) once per tagged rule. The arguments come from the parsed tuple, so it works after a network change. Then it re-reads `user.rules`.
- Never by rule number: that races with other tools.
- Never automatic at session end: that would prompt on every cast.
- Changing `cast.media_port` offers to move the rule, as one remove and one add under the same polkit keep.

**Another network.** The rule is scoped to interface plus subnet, so it survives DHCP changes. On a different Wi-Fi or Ethernet, detection says `Blocked`, and the button adds a rule for the new network. The old rule stays, listed as "Allowed on another network", until Remove.
- A laptop elsewhere on another 192.168.1.0/24 would match too. That is mitigated: the server listens only during a session, with tokens and a peer check, and Remove is one click.

**Copyable fallback.** It is always available under **Show command**. It is the only remedy for firewalld, nftables, `NoAgent` and `NotAuthorized`.

```
sudo ufw allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment 'omatuner cast'
# remove:
sudo ufw delete allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp
# firewalld (copy only; not verified on this machine):
sudo firewall-cmd --permanent --add-rich-rule='rule family=ipv4 source address=192.168.1.0/24 port port=47800 protocol=tcp accept' && sudo firewall-cmd --reload
```

- The tag is `omatuner cast` everywhere: the button, the copy, detection and Remove. ufw stores it as hex `6f6d6174756e65722063617374`.
- The command contains this computer's own interface and subnet, which may be painted (§14.6). It never contains the TV's address.

**Agents never touch the firewall.** No tool runs `pkexec` or returns a command. The `fix` skill points the user to the button.

---

## 10. Provider connection limits

- **Casting runs inside picture A's controller, so the relay's single upstream connection is A's lease.** `SharedBudget` is keyed by `TileId`, so no new holder type is needed.
- Multi-view is off while casting, so B can't compete for the playlist.
- **Order on cast start:**
  1. mpv stops through `must_stop_first` (END_FILE awaited for ≤ 1.5 s on limit-1 or unknown-limit playlists);
  2. the controller acquires the lease;
  3. the relay opens upstream.
- **The TV never opens a provider connection.** Its retries and stalls hit the relay, not the provider. The probe shares the head buffer, and HLS upstream is fetched one variant, one request at a time (ffprobe and ffmpeg both read it with `-http_multiple 0`). A request connects only once the previous one's socket has closed, and `UpstreamClosed` comes after that close: reqwest closes a socket on hyper's connection task after the response is gone, so the relay's provider connections live on a runtime of their own and the relay waits until none is left there.
- **Relay reconnects** are the controller's `Effect::Load` under the existing policy, including the 45 s `race_window` and `race_retry` (`reconnect.rs:44-45`).
- **Copy fallback still works.** `CopyPlay.elsewhere` for 401/403/Budget and `observe_refusal` act on `Input::HttpStatus`, which the relay now supplies.
- **Mirroring (local plus TV) is not offered.** It needs a second lease, and on these accounts it is usually refused.

---

## 11. Player state while casting

### 11.1 Snapshot

`state`, `playable`, `position`, `duration`, `paused`, `seekable`, `origin`, `last_error` and `load_generation` keep their meanings, fed from the remote events. `cast: Some(CastView)` is the switch the UI reads. The mpv-only fields stay at their defaults (§5.5).

### 11.2 Remote event → `state::Input` (`player/remote.rs`, pure, unit-tested)

| `RemoteEvent` | Input(s) |
|---|---|
| `UpstreamOpened { gen }` | `StartFile { entry: gen }` |
| first `Playing { gen }` | `FileLoaded` + `PlaybackRestart` (first frame) |
| later `Playing` / `Buffering(false)` | `PausedForCache(false)` |
| `Buffering(true)` | `PausedForCache(true)` |
| `Position { pos, dur }` | `TimePos(pos)` (absolute: the session adds the item's offset), and `duration`. From `playbackInfo` or the estimate (§7.3). |
| a failure of the generation (`UpstreamClosed` before EOF, `Failed`, `Lost`) | `TimePos(the TV clock)` first: the last report run on while the TV played and held while it was paused or buffering. The TV plays its buffer on after a provider drop, so the reconnect's resume point (the machine's `last_pos − 2`) is where the viewer is, not the last report (up to 10 s old). Relative seeks count from the same clock. |
| `Paused(b)` | `Pause(b)` |
| `Http { status }` then the upstream ends | `HttpStatus(status)` then `EndFile { reason: Error, entry: gen }`: the same sequence mpv produces today |
| `ProviderText { text }` | `ProviderMessage(text)` |
| upstream EOF | `EndFile { reason: Eof }`. Live: `StreamEnded`, then the reconnect policy. VOD: wait for the receiver. |
| `Ended { gen }` (itemPlayedToEnd) | `EndFile { reason: Eof }`, then `Ended { finished }`, then Up next |
| `Lost(StoppedOnTv)` | the controller ends casting (output Local, `Input::Stop`). No error state; the alert of §3.4 offers Play here. |
| `Failed { kind }`, other `Lost(kind)` | `Fail(PlayerError { kind: Cast(kind) })`. Reconnect: `Unreachable` re-verifies once; the others don't retry. |
| `UpstreamClosed { gen, at }` | the controller releases the lease and records `released` (no Input) |

### 11.3 Commands

| Command | AirPlay (Phase 1) | Cast / DLNA (later) |
|---|---|---|
| `TogglePause`, `SetPause` | `setRate 0/1`. Live pause is limited to the window (about 80 s); resuming after that reloads at the live point. | PAUSE/PLAY · Pause/Play |
| `SeekBy`, `SeekTo` (only when `snap.seekable`) | Replace (§7.3). Inside the produced part, with a playlist view starting at the target's segment. Beyond it, VOD restarts ffmpeg with `-ss` and a new token. Takes 2–5 s; the card shows "Seeking…". | SEEK · Seek REL_TIME |
| `Stop` | `setRate 0`, TEARDOWN, the session closes, the relay 404s, the output goes back to Local, then Idle | same |
| `StopCasting { play_here }` | §5.5 | same |
| `Zap`, `ZapTo`, `LastChannel`, `JumpToLive`, `Play`, `PlayCopy` | Unchanged up to `Effect::Load`, then `RemoteCmd::Load` → replace | same |
| `SetVolume`, `AdjustVolume`, `ToggleMute`, `SetMute` | Banner "Use the TV remote for volume". The snapshot volume is left alone. | Receiver level: Tuner 0–130 maps to `min(v,100)/100` |
| Audio or subtitle track, aspect, zoom, sub delay or scale, `SetLightDecode`, `ResumeVideo` | No-op, banner "Not available while casting". The controls are hidden. | Phase 2: track choice is a reload with `-map` |
| `RefreshStreamInfo` | What the TV is fed, from the relay: it reports it at every launch (`RelayEvent::Probed`, kept per generation), probed or not — the hints often suffice and the probe is skipped (§8.1.2), which in 0.3.0 left the panel on "Reading stream info…". `StreamInfo::cast` (`CastFeed`): from the probe or the hints, the level, the picture's way (copied / converted, deinterlaced), the sound's (copied / AAC stereo), surround (copied / AC-3 5.1), fMP4 or TS, the pinned target duration. The panel adds the TV and its model, behind-live, and the position source (`CastApi::position_source`: the TV's `playbackInfo` or the estimate) | same |
| `Reconnect` | Restart the pipeline and replace, where the TV clock says the TV is (§11.2) | same |
| `ApplyConfig` | Applied to the local player. `[cast]` changes take effect on the next load. | — |
| `Shutdown` | Fast stop (≤ 300 ms), then the session closes | same |

### 11.4 What other subsystems get

- **Up next.** It watches `origin == Queue` and `Ended` as today, so "next episode on the TV" works. It still waits in multi-view, which can't be on while casting. Its PIN-wait notification follows §3.8.
- **Progress and history.**
  - `save_progress` (`controller.rs:1126`) runs every 10 s from the TV's positions.
  - `finished` comes from `duration` as today.
  - `record_video` is skipped while casting: there are no measured dimensions.
- **MPRIS.** Unchanged; the media keys drive the TV. A volume change through MPRIS is a no-op on AirPlay.
- **Sleep timer.** Stop and SetPause reach the TV.
- **Live preview autoplay.** Suppressed while `snap.cast.is_some()` or while Play on is a TV (§3.9; `live.rs`, `guide.rs`). Otherwise moving the cursor in Live would retune the TV. The preview pane shows the casting card. The controller never casts a `LivePreview` play either.
- **Parental.** Relock stops a cast that is showing locked or explicit content, exactly as it stops the local picture (`sync_explicit`).
- **Reminders.** A switch while casting retunes the TV and skips its "Open in" placement (§3.8).

### 11.5 Power

- **Wayland idle inhibitor.** Omarchy's inhibitor (`platform/idle.rs`) is a Wayland surface inhibitor. It is released while casting, so the laptop screen may lock and blank.
- **logind sleep inhibitor.** Without one, a suspend kills the relay. While casting, Tuner takes `Inhibit("sleep", "Tuner", "Casting to Living Room", "block")` through `zbus`, which is already in the tree. It is controlled by `cast.keep_awake`, on by default. It holds whatever workspace Tuner is on, and with the screen locked. A film that played to its end lets it go (the computer may sleep again with nothing playing); the next load on the TV takes it back.
- **Lid switch.** Closing the lid still suspends (§17).

---

## 12. Agent and MCP (`src/agent/`, plugin skills)

**`cast_devices` (new, read-only, query bucket)**
- Returns `{devices: [{ref: "device:<32 hex>", name, kind: apple_tv|airplay_tv|chromecast|dlna, paired, available, casting}]}`.
- Reads the registry, which includes paired devices from the store. It never starts a scan.
- Refs are opaque (DESIGN §30.3), and no addresses are returned.

**`play` gains `device`** (a ref from `cast_devices`, or an exact name)
- Sends `CastTo { device, play: Some(req) }`. Connecting can exceed the 4 s `HANDLER_BUDGET`, so the reply is immediate: `outcome: casting_started`. The agent confirms with `status`.
- **Placement (§3.8).** A play with `device` skips `act::agent_show`'s window placement: no pop-out, no raise, whatever `open_in` or "Agents play in" says. `PlayOut.opened_in = "device"` plus `device: <name>`. A popped-out window is put back in, unraised.
- Outcomes:
  - `pin_required`: a locked item. The PIN is asked on Tuner's screen as today, raising the window (§30.13 step 2). Nothing is cast before the unlock.
  - `pairing_required`: "Pair Living Room in Tuner (Settings ▸ Cast)". Agents never pair; no tool takes a PIN.
  - `unavailable`, with a typed reason: `cast-unreachable`, `cast-firewall`, `cast-unsupported` or `multiview-on`.
  - `cast-firewall` only from a fresh detection made toward that TV's own address (`firewall_for`, waited for up to 1 s), which says `Blocked` (§9.4 "Toward which TV"). No detection in time, an unknown state, or one made toward another address blocks nothing: the session's 10 s fetch check is the authority for a real block.
- **Tuner not running.** The bridge starts Tuner's window, as for any play that doesn't open in the pop-out (§30.13). It never uses `--agent-popout` for a cast.
- **Bridge (`agent/mcp.rs`).** Unlike `open_in`, `device` must **not** be retried without the field on an older Tuner. Playing locally instead would be wrong. The bridge returns "The running Tuner can't cast; update Tuner".

**`control`**
- Gains `stop_casting` and `play_here`.
- `pause`, `resume`, `stop` and `volume` already reach the TV through handle A. On AirPlay, `volume` answers `unsupported`.
- `popout` and `window` while casting answer `unavailable: casting`.

**`status`** gains `casting: {device, kind, phase, behind_live_s?, error?}`, and `play_on: {ref, device}` while the Play on switch names a TV (§3.9: a `play` without `device` then goes there, `opened_in: "device"`). `error` is the typed reason of the last failure (`cast-firewall`, `cast-stopped-on-tv`, …). It lets an agent explain a failure while Tuner is out of sight (§3.8).

**`diagnose`** gains a `cast` section, with no addresses:
- the receiver model and tvOS version (§7.6);
- the pairing variant;
- `timing: ptp-declared`;
- the firewall state (§9.4);
- the last error kind.

**Consistency.** `Call`, `from_tool`, `name`, `is_action`, `tools()` and `dispatch` are updated together; the `tools.rs` test enforces this. Casting calls are actions (the 2/s bucket).

**Guards**
- Replies pass `guard_reply` then `sanitize`. Errors are typed kinds, never device text (§30.6).
- Actions are logged at INFO by method and ref only, with no device names or IPs.

**Instructions and skills**
- MCP server instructions get the line: "Starting a cast on a TV needs the user's yes: naming the TV is the yes, and so is asking for 'the TV' when exactly one TV in `cast_devices` is paired (cast to it, no question); with two or more paired, ask which. Unpaired TVs don't count."
- **Which TV "the TV" is.** The agent counts only `paired` TVs. Exactly one: it casts to it, and the request is the yes, as naming the TV is. Two or more: it asks which. An unpaired TV (one that is discovered, not paired) never makes "the TV" ambiguous. With none paired, "the TV" means what it did with no TV listed: `full_window` when `cast_devices` is empty, and `pairing_required` when a TV is listed but not paired. This is the agent's protocol (rules, skills and tool descriptions): the server resolves no "the TV" and enforces no confirmation; `play {device}` always names its TV.
- `watch` gets one "put it on the TV" line.
- `kids` notes that parental locks apply on the TV too.
- `fix` reads `status.casting` and the typed errors. For `cast-firewall` it tells the user to press **Allow on firewall** in Tuner (Settings ▸ Cast ▸ Network). No tool changes the firewall, and agents never run `sudo` or `pkexec`.

---

## 13. Config, storage and packaging

**`[cast]` in `src/config.rs`** (frozen; sign-off). Machine-local, kept by `Config::restored` like `[agents]`. No secrets, no addresses.

```toml
[cast]
enabled = true
offer_setup = true
checked_version = ""
default_device = ""          # DeviceId hex; empty = ask
media_port = 47800           # TCP; the only port the firewall needs
min_segments = 2             # segments ready before the TV is told to play (VioletRelay: 2 × 4 s)
convert = "auto"             # auto | never
interlaced = "auto"          # auto (deinterlace for AirPlay, Apple HLS rule 1.14) | as-is
keep_awake = true            # logind sleep inhibitor while casting
```

- Revision 1's `timing_port`, `airplay_timing` and `audio` are gone: timing is PTP only, and surround is automatic (§8.4: a stereo group is always there, AVPlayer picks).
- The test builds' `experimental` is gone too: third-party AirPlay TVs are always listed. A config that still has it loads (unknown keys are ignored).
- Dev-only environment variables, never in config:
  - `TUNER_CAST_TRACE=1` (the redacted transcript, §7.3);
  - `TUNER_CAST_LIVE=<ip>` (the ignored LAN smoke test);
  - `TUNER_CAST_PYATV=<venv python>` (the ignored interop test).

**Store (migration `0014_cast.sql`; `store/migrations.rs` append with sign-off)**
- `cast_identity`, `cast_device` (§7.5).
- `cast_hint(item, receiver, level, interlaced, vcodec, acodec, irregular, updated_at, PRIMARY KEY(item, receiver))`.
- None of them is exported by `backup.rs`.

**Paths (`src/paths.rs`, frozen)**
- `cast_live_dir()` = `runtime_dir/cast`.
- `cast_vod_dir()` = `cache_dir/cast`.
- `cast_trace_dir()` = `state_dir/cast-trace` (dev only).
- All are created 0700 and swept at startup (the trace directory keeps its last 5 files).

**Cargo (`Cargo.toml`/`Cargo.lock`, frozen; one `skeleton-v1.N` tag)**

| Change | Licence | New compile? |
|---|---|---|
| `mdns-sd = { version = "=0.21.4", default-features = false }`. The newest release ≥ 14 days old; 0.21.5 is from 2026-10-05. | Apache-2.0 OR MIT | yes (small: flume, if-addrs, socket-pktinfo) |
| `num-bigint` (newest ≥ 14 days old), for SRP | MIT OR Apache-2.0 | yes (small) |
| `axum` promoted from dev-deps, `default-features = false, features = ["http1", "tokio"]` | MIT | no (already built for tests) |
| `aws-lc-rs` direct, same version and features as rustls' (`cargo tree -e features`) | ISC AND (Apache-2.0 OR ISC) | no |
| `zbus` direct, same version as mpris-server's, for the logind inhibitor | MIT | no |
| `libc` direct (`prctl(PR_SET_PDEATHSIG)`) | MIT OR Apache-2.0 | no |
| `if-addrs` direct (same version as mdns-sd's), for the interface name and subnet in the firewall rule | MIT OR BSD-3-Clause | no |
| tokio feature `process` | MIT | tiny |
| `[[bin]] name = "tuner-firewall", path = "src/bin/tuner-firewall.rs"` (std only, no dependencies) | GPL-3.0-or-later (ours) | tiny |

Not needed: protobuf/prost, `plist`, dalek/RustCrypto, `srp`, `rust_cast`, `rupnp`, OpenSSL, avahi, Python. All of the above are compatible with GPL-3.0-or-later, and the licence goes in a trailing comment on each line.

**PKGBUILD**
- Add `ffmpeg` to `depends`. It costs nothing (`mpv` already depends on it) and declares the CLI dependency.
- Add `optdepends=('polkit: the "Allow on firewall" button for casting')`.
- Install the helper and its policy:
  - `install -Dm755 … /usr/lib/tuner/tuner-firewall`;
  - `install -Dm644 packaging/polkit/io.github.rthoms334.omatuner.firewall.policy /usr/share/polkit-1/actions/`.
- Install `THIRD_PARTY_NOTICES.md` beside `LICENSE` in `/usr/share/licenses/omatuner/`.
- Nothing changes for either architecture beyond the existing aws-lc `_cflags()` handling.
- `omatuner.install` echoes one hint on the upgrade that brings casting: "Cast to your TV: Settings ▸ Cast (ufw users: Tuner offers an Allow on firewall button)". It runs nothing.

**CI.** No new packages: `ffmpeg` is already installed for test media. Nothing in the default test path touches multicast, the real firewall or `pkexec` (tests use a fake `pkexec` path).

---

## 14. Testing (meaningful only; no fixed-sleep negatives)

### 14.1 Pure unit tests (fast, in-module)

1. **Pairing primitives:**
   - TLV8, including fragmentation and unknown tags;
   - SRP-6a against the RFC 5054 Appendix B vectors (hash made generic), plus the 3072/SHA-512 exchange from the pyatv golden vectors (§14.3);
   - HAP frame encrypt/decrypt golden bytes, including inbound frames of 1500 B and 16 KiB, a bad tag and an over-cap length;
   - pair-setup M5/M6 and pair-verify with fixed keys, checked against the pyatv vectors;
   - the M4 proof and M6 signature checks rejecting forged input.
2. **Decoding:**
   - bplist round-trip and golden bytes, plus caps against hostile input;
   - TXT and `/info` feature, flag and version decoding on the two real plists. Assert:
     - the Apple TV is video-capable, with pairing mandatory and version 26.6;
     - the Hisense is a usable third-party AirPlay TV, with bits 49 and 33 and no bit 0;
     - a Mac is unsupported;
     - both the TXT (0x18644) and `/info` (0x118644) flag forms decode.
3. **Transport:** RTSP/HTTP mixed-response matching; a stray late CSeq dropped; one-in-flight enforced; event reassembly (multi-frame, CSeq ack).
4. **Session builders (`queue.rs`):** the item dicts per profile; `playerLoggingID` ≤ 6; `X-Apple-StreamID` spelling; the replace sequence; no NTP anywhere.
5. **Pipeline decisions:**
   - `plan()` over every row of §4.2 and §8.3 (interlaced → L3 for AirPlay, "as-is" → L2);
   - playlist guard (TARGETDURATION pinning; IDR check on committed TS fixtures: IDR, recovery-point I, interlaced, LATM, MP2);
   - playlist views (sequence number, PDT kept, grows only at the end);
   - the remote event → `Input` adapter (§11.2);
   - the position estimate (pause, loading, bound by fetched segments).
6. **Small parsers:** `sanitize_name`; the token route matcher.
7. **Alerts (§3.8):** a pure `alert_route(seen, player_on_screen, notify_ok)`.

### 14.2 Relay over loopback (`tests/cast_relay.rs`)

- **Upstream:** `TestServer` (`tests/support/media.rs:241-340`) serving `bars.ts` and `hls/`. Real ffprobe and ffmpeg, with `-hls_time 1` in tests. A reqwest "TV" fetches the playlist and segments.
- **Playlist correctness:**
  - at least 6 segments eventually;
  - `PROGRAM-DATE-TIME` on every segment;
  - a stable TARGETDURATION;
  - no 3xx;
  - CORS present;
  - a media playlist, never multivariant.
- **Server rules:**
  - a wrong token, a wrong peer (allow-list injected) and path traversal all get byte-identical 404s;
  - HEAD and Range give 206 and a correct `Content-Length`;
  - a view token serves the shared segments and the old token 404s.
- **One connection:** `set_session_limit(1)` and `max_concurrent == 1` across probe and stream; a request cut mid-body (throttled HLS segments) closes its socket before the next one connects, and none is left open when `close` returns.
- **Reconnect:** 403 becomes `RemoteEvent::Http(403)`; `kill`/`revive` produces upstream close and reopen.
- **No leaks:**
  - no provider host, path or credential string in any served byte, header or log line (the `tests/redact.rs` pattern);
  - `/proc/<ffmpeg pid>/cmdline` holds no upstream host.
- Skipped through `ensure_media()` when ffmpeg is missing locally; fails loudly in CI.

### 14.3 Fake receiver (`tests/support/fake_airplay.rs`, via `#[path]`)

No open-source receiver implements the `/command` play queue: foxCast's receiver and pyatv's fake both implement only the legacy `/play`. So the fake is assembled from the references' own fakes, tests, captures and published notes [FAKE]:

| Piece | Derived from |
|---|---|
| Server-side pair-setup (`/pair-pin-start`, M1–M6) and pair-verify (M1–M4), control keys swapped server-side | pyatv [`protocols/airplay/server_auth.py`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/pyatv/protocols/airplay/server_auth.py) (MIT, 492 lines), **ported and made strict**: pyatv's server doesn't check the client's SRP proof or its Ed25519 signatures, and ours does, as a real TV does |
| What "strict" means | foxCast's receiver test list (scenarios only): rejects a wrong SRP proof, rejects a forged controller signature, wrong PIN at M3 → Error TLV 2, 453 on pin-start accepted |
| State plus scenario knobs, injectable failure counters | pyatv [`tests/fake_device/airplay.py`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/tests/fake_device/airplay.py) (`FakeAirPlayState` / `UseCases` split) |
| SETUP, RECORD, type-130 SETUP and the `/feedback` reply `{streams: []}`; the event channel's `updateInfo` and its required RTSP 200; HKDF label tables | pyatv [`docs/documentation/protocols.md`](https://github.com/postlund/pyatv/blob/b277a4c8222ecdcbaab8a24e3e713ca44765adb4/docs/documentation/protocols.md) L1525–1979, the only published wire samples |
| `/command` decode, command order, headers, "a 500 stops the sequence", event shapes `{params:{data}}` / `params.playbackState` | robkochman `tests/protocols/airplay/test_airplay_v2_player.py` (MIT) |
| Client-name regression (header on pin-start and all three pair-setup requests) | PR 2894's fake-receiver test (MIT) |
| Quirks: `playerLoggingID` > 6 → silent close; a `/command` while `/feedback` is in flight → close; NTP → no events, then close | PR 2899 |
| Event channel: split command acked once; invalid frame rejected | foxCast `event_channel_test.go` (scenarios only) |
| State and notification names | foxCast `docs/05` |
| `playbackInfo` response (`kind: response`, `messageID`, `value{position, duration, rate…}`) | PR 2846; keys from LIVI (facts) |
| Pairing message order and sizes (M2 is 409 B) | `openairplay/ap2-sender` `pairing.txt`, checked, not committed (no licence) |
| `/info` | our scrubbed LAN captures (§5.3) |

**Behaviour**
- **Transport.** It listens on 127.0.0.1:0 and starts in plaintext. After pair-verify it switches to HAP framing. One connection carries mixed HTTP/1.1 and RTSP/1.0, and RTSP replies go by CSeq.
- **SETUP (base)** answers `{eventPort[, skipRecord]}` and opens the event listener, with the `Events-Salt` keys swapped. It never sends a `timingPort` for PTP.
- **Event channel.** After RECORD, or after SETUP when `skipRecord` is set, it sends `POST /command updateInfo`. It requires `RTSP/1.0 200` with the CSeq, and closes after `ack_timeout` without one (default 30 s; tests use 1 s).
- **`/command`** answers 500 before the type-130 SETUP. Type 130 answers `{streams:[{type:130, streamID:1}]}`, with no `dataPort`.
- **The `/command` decoder** handles:
  - `insertPlayQueueItem`. It checks `uuid`, `Content-Location`, `mediaType "file"` and `playerLoggingID` ≤ 6, and closes silently if that is longer;
  - `setProperty`, `setRate`, `removePlayQueueItem` with `itemAfter`;
  - `playbackInfo`.
- **TV-like fetching.** After the insert and `setRate 1`, it fetches `Content-Location` with an `AppleCoreMedia/1.0.0.23L773 (Apple TV; …)` User-Agent, AVPlayer-style:
  - `Range: bytes=0-1` first, then the playlist, then the segments;
  - it emits `loading` → `playing` on the first segment;
  - it emits `itemPlayedToEnd` after the last segment of an ENDLIST playlist.
- **TEARDOWN** closes the event channel. A `stop_from_remote()` hook closes the control connection after `playing`, which is what the remote's Menu does.
- **Request log.** Every request is recorded with its decoded plist bodies, so tests assert exact sequences, and assert that `Content-Location` is a relay-token URL and never a provider URL.

**Modes (`FakeConfig`)**

| Knob | Values | Models |
|---|---|---|
| `tvos` | `V26` (the 26.6 `/info`), `V27` (no PIN without `X-Apple-Client-Name`; `sourceVersion 1005.12.1`; an 8931-B event frame) | §7.6 rows 1, 7 |
| `pairing` | `Hkp3`, `Hkp5Only` (470 to HKP 3 at M3) | the variant ladder |
| `allow_access` | `SameNetwork`, `HomeOnly` (empty 200 on pair-verify), `Refuse470` | §7.6 row 3, §3.2 |
| `m6_tlv17` | bool (a 159-B type-17 TLV) | §7.6 row 5 |
| `skip_record`, `record_status` | `Option<bool>`, u16 | §7.6 row 8 |
| `split_events` | bool (1024 + 1259-B frames) | §7.6 row 6 |
| `playback_info` | `Answer`, `Status500`, `Silent` | §7.3 position |
| `fetch` | `Normal`, `Never` (firewall-blocked), `Slow` | the 10 s check, escalation |
| `profile` | `AppleTv`, `ThirdParty` (needs the type-120 SETUP; rejects `X-Apple-ProtocolVersion` on `/command`; no `psi` in `/info`; checks that `SETPEERSX`'s `ClockPorts` is keyed by the TXT `gid`) | §7.7 |
| `strict_ntp` | bool (an NTP declaration → no events, close after 2 s) | §7.6 row 9 |
| `late_feedback_reply` | bool (answers one `/feedback` late) | the stray-CSeq guard (#2893) |

**Anchors against self-consistency.** Our client and our fake could agree and both be wrong. Four things guard against that:
1. **Golden vectors from pyatv 0.18.0.** `scripts/cast/pyatv_vectors.py` runs in a throwaway venv with `python -I`, using pyatv from PyPI. With fixed seeds and the PIN 1111, it writes `tests/fixtures/cast/vectors/pyatv_0_18_0.json` (MIT attribution in the header). The file covers:
   - TLV8 encodings;
   - SRP values (salt, A, B, M1, M2, K);
   - every HKDF label's output;
   - pair-setup M5/M6 and pair-verify M1–M4 bytes;
   - HAP frames.
   
   Both the client and the fake are checked against them.
2. **The RFC 5054 Appendix B vectors** check the generic SRP maths.
3. **Dev-only interop.** `scripts/cast/pyatv_interop.py` backs `#[ignore]` tests gated on `TUNER_CAST_PYATV`:
   - our client pairs and verifies against pyatv's own `AirPlayServerAuth`;
   - pyatv's client pairs and verifies against our fake.
   
   They are run once in CAST-3 and again at the gate.
4. **The real TV at the end (§18).** The first session runs with `TUNER_CAST_TRACE=1`. In a follow-up commit the redacted transcript becomes `tests/fixtures/cast/trace_tvos*.json`, which the fake can replay.

The two sides are built by different agents from this document: the client in CAST-2 and the fake in CAST-3. That way they don't share a misreading.

### 14.4 AirPlay against the fake (`tests/cast_airplay.rs`)

Every scenario runs in both `Tvos::V26` and `Tvos::V27`.
- **Pairing:**
  - HKP 3 happy path;
  - the ladder to HKP 5;
  - the client-name header on all four requests;
  - pin-start and pair-setup on one TCP connection;
  - a wrong PIN → `WrongPin`;
  - Error 3 → backoff;
  - 470 on both variants → `PairingRefused`;
  - a type-17 TLV in M6 tolerated;
  - a forged M6 signature and a bad M4 proof rejected.
- **Verify:**
  - a changed TV key → `PairingLost`;
  - an empty 200 → `PairingRefused` with the "Home only" hint.
- **Session:**
  - the exact request sequence of §7.3;
  - never NTP;
  - `skipRecord` honoured, and a RECORD 500 tolerated;
  - `/command` never overlaps `/feedback`;
  - feedback starts only after `setRate 1`;
  - `Content-Location` is the relay URL.
- **Event channel:**
  - every request acked with its CSeq;
  - a 1024 + 1259 split reassembled and acked once;
  - an 8931-B frame read;
  - a bad tag → `Lost`.
- **State:**
  - `playing` → Playing;
  - `itemPlayedToEnd` → Ended;
  - stop from the remote → `Lost(StoppedOnTv)`;
  - `playbackInfo` answered → Position;
  - a 500 or silence → the estimate.
- **Replace:**
  - the VioletRelay sequence, then `actionAtItemEnd 1` after the new item's first fetch;
  - a failed replace → a full session restart.
- **Stop:** `setRate 0`, TEARDOWN, everything closed.
- **Firewall-blocked fake:** the 10 s check gives `Firewall` with detection `Blocked`, and `ReceiverFailed` with `Allowed`.
- **Third-party profile:**
  - type 120 before 130;
  - header negotiation;
  - `HLS-Content-Location` present;
  - `channelID` from the MAC.

### 14.5 Controller (fake `CastConnector`, `TableResolver`/`RecordingStore` from `tests/player_mpv.rs`)

- `CastTo` stops local mpv before the remote `Load`, and the lease is held once by A.
- A zap sends a new `Load` with a new generation.
- Progress is saved from remote positions; `Ended { finished }` advances the queue.
- A sleep-timer Stop reaches the remote and leaves the player Idle.
- `Lost` gives an Error with Play here, and `Lost(StoppedOnTv)` goes Idle without an error.
- `StopCasting { play_here }` resumes locally at the remote position.
- The route refuses multi-view while casting.

### 14.6 UI (kittest through the `home.rs` testkit `rig()`/`until()`, one rig at a time)

- The picker lists `FakeDiscovery` devices with the right state lines.
- The PIN overlay arms after 600 ms; **Show a new code** asks for the next variant.
- The casting card shows the device and phase, with Stop casting and Play here.
- The OSD has a control labelled "Cast to a TV", with `osd_timeout_ms` set explicitly.
- The first-run card respects `CardGate` and waits for the agent card; "Don't ask again" persists.
- Settings ▸ Cast rows render, the firewall row shows Allow or Remove by state, and the TV row shows its version.
- The firewall card goes through "Waiting for your password…" and maps each `AllowOutcome` to its copy, using the fake `pkexec`.
- **Workspace (§3.8):**
  - a cast that starts while popped out pops back in, unraised;
  - an agent `play {device}` neither pops out nor raises;
  - a cast error with the window "not seen" goes to the notification path (injected), and with it seen goes to the card or a toast.
- **`painted()` never contains `://`, a device IP or a token.** The computer's own subnet in the firewall command is allowed.

### 14.7 Firewall (`tests/cast_firewall.rs` + unit tests in `src/bin/tuner-firewall.rs`)

- **Parser on fixture files** (`tests/fixtures/cast/ufw/`):
  - this machine's `user.rules`, scrubbed (LocalSend and docker-dns rules);
  - a tagged rule;
  - a rule tagged for another interface;
  - a shadowing earlier `deny`;
  - an app-profile rule;
  - a route rule;
  - port lists and ranges;
  - `any` proto;
  - `_log` actions;
  - a non-hex comment.
- **State mapping:** `ENABLED=no`, the unit inactive, policy ACCEPT, firewalld active (injected `systemctl`).
- **argv builders:** the helper form and the checkout form, both shell-free, and the copy text.
- **The runner against a fake `pkexec`** (a script in a temp dir that records its argv and exits 0, 126, 127 with each stderr text, 3, or sleeps for the timeout):
  - each `AllowOutcome`;
  - Cancel kills the child;
  - a re-read after success.
- **Helper validation:** bad interface, public or too-wide CIDR, host bits set, low port, unknown verb, not root → exit 2 with no ufw call (ufw is injected as a recording stub path in tests).

### 14.8 Agent (`tests/agent_app.rs` `World`)

- `cast_devices`.
- `play { device }`:
  - on an unpaired device gives `pairing_required`;
  - on a locked item gives `pin_required` and raises the window;
  - otherwise gives `opened_in: device` with nothing raised.
- `control stop_casting`; `control popout` while casting → `unavailable`.
- `status.casting`, including `error`.
- `diagnose` has the cast section and no address.
- The wire never contains `://`.

### 14.9 Rules

- **No real network in default tests.** No real multicast and no ports 5353, 7000, 8009 or 1900. `off_desktop()` gives `NullDiscovery`.
- **Waits** are bounded condition polls, and negative checks drain after a store `blocking` barrier.
- **Ignored tests.** A soak test, if any, is `#[ignore]`.
- **No real privilege.** Tests never call the real `pkexec`, `ufw` or `systemctl`; their paths are injected.
- **One live smoke test.** `cast_lan_smoke` is `#[ignore]` and gated on `TUNER_CAST_LIVE=192.168.1.5`, for manual runs.
- **Store:** a backup export contains no `cast_*` rows.

---

## 15. Phased plan

### 15.0 How Phase 1 is verified (there is no Phase 0)

Revision 1 gated the code on a 2–3-day hardware spike on the user's Apple TV. The user decided against it: "This is a solved problem in terms of airplay and casting in general. We should not need to do spikes and blind builds." Phase 1 is verified in this order instead:

1. **Ported, not discovered.** Every protocol step is ported from a reference with hardware evidence (§7.0). Where the references differ, §7 picks one and cites the reason, and takes a tolerant path where the user's TV could still differ.
2. **Anchored.** Golden vectors come from pyatv 0.18.0 and RFC 5054, plus the dev-only interop against pyatv's own server and client (§14.3).
3. **Faked from the references.** The fake receiver runs in tvOS 26 and 27 modes, and the relay runs against real ffmpeg (§14).
4. **One combined gate:** `cargo fmt --check`, `clippy --all-targets -D warnings`, `build`, then `dbus-run-session -- cargo test`, then the release CI watched with `gh run watch` exit codes.
5. **The user's TV, once, at the end.** The user runs §18. The first session runs with `TUNER_CAST_TRACE=1`, and the redacted transcript becomes regression fixtures in a follow-up commit. Nothing in Phase 1 waits on it.

Every default the spike was going to set now comes from a source:

| Default | Value | Source |
|---|---|---|
| timing | declared PTP | PR 2899, foxCast, VioletRelay |
| interlaced | deinterlace | Apple 1.14 |
| `min_segments` | 2 | VioletRelay |
| step order | RECORD, then `/info` | PR 2899, foxCast, castcli |
| start key | none needed | playlists start at 0, §8.5 |
| surround | Phase 1: a surround rendition beside the stereo one | §8.4, §17 |
| Hisense | third-party profile, listed like the Apple TV | VioletRelay |

No provider URL is recorded, and no throwaway venv runs against the TV.

### 15.1 Phase 1 scope: AirPlay to the Apple TV

**In scope**
- **Discovery:** on-demand mDNS, plus manual address. Third-party AirPlay TVs are listed like the Apple TV. Receiver versions are read.
- **Pairing and storage:**
  - PIN pairing in Tuner, with the HKP 3 → HKP 5 ladder on one connection;
  - credential and hint tables.
- **AirPlay:**
  - the §7.3 session with declared PTP, the replace sequence and the event channel;
  - `playbackInfo` with the estimate fallback;
  - tvOS 26 and 27 behaviour (§7.6);
  - the third-party profile;
  - the dev trace.
- **Relay:**
  - the loopback upstream proxy with head buffer and HLS variant pick;
  - L2 and L3;
  - the playlist guard and playlist views;
  - the LAN server with tokens and a peer check.
- **Content:** live, catch-up (EVENT; seek inside what's produced) and VOD (EVENT; `-ss` resume and seek). HEVC goes to fMP4, and interlaced input is deinterlaced.
- **Controls:** pause, resume, stop, seek by replace, zap. Queue, MPRIS, the sleep timer and relock all act through A.
- **UI:**
  - the casting card and the error states;
  - the OSD control, the menu items and `C`;
  - the picker and the PIN overlay;
  - the mini-player glyph and the status chip;
  - pop-out disabled, and the quit confirmation.
- **Workspace behaviour (§3.8):** casting never moves the window, and alerts go to the card, a toast or a notification.
- **Setup surfaces:**
  - the first-run card and wizard with Test on TV;
  - Settings ▸ Cast;
  - firewall detection, **Allow on firewall** (pkexec + helper + polkit policy), **Remove rule**, and the copy command.
- **Agent:** `cast_devices`, `play.device`, `control stop_casting/play_here`, `status.casting` (with `error`), the `diagnose` cast section, the skills lines.
- **System:** the logind sleep inhibitor, `ffmpeg` in `depends`, the helper and policy in the package, the third-party notices.
- **Docs:** DESIGN §40, plus updates to §11.5, §13.8, §15.2, §20, §21, §22 and §30; the README.

**Not in Phase 1:**
- Google Cast, DLNA, L1 pass-through;
- local files;
- audio-track choice, subtitles;
- seamless zap;
- TV volume;
- catch-up seek beyond what's produced;
- guide/browse "Play on …" menus;
- an NTP responder or PTP follower.

### 15.2 Phase 1 work items

These follow the multi-view pattern.

**Process**
1. A skeleton tag (`cast-base`) first commits every frozen change and every cross-area signature.
2. The items then run in parallel, with disjoint file ownership.
3. Agents don't run the full build. Each may run `cargo check` and its own test target.
4. **One combined gate, review and fix comes at the end** (§15.0 step 4).
5. **Then the user runs §18 on the TV.**

**Model policy (user):** Sonnet 5.5 for routine work, Opus 5.5 for design and hard reasoning. Effort is low, medium, high, xhigh or max.

| ID | Work item | Files (owned) | Depends on | Size | Model · effort |
|---|---|---|---|---|---|
| **CAST-0** | **Skeleton (architect).** Frozen changes plus contracts with SKELETON SAFE DEFAULT bodies. | `Cargo.toml` (deps, `[[bin]] tuner-firewall`), `Cargo.lock`, `src/lib.rs` (`pub mod cast`), `src/model/cast.rs` + `src/model/mod.rs`, `src/config.rs` (`CastConfig`, `restored`), `src/paths.rs`, `src/store/mod.rs`, `src/store/migrations.rs` + `migrations/0014_cast.sql`, `src/ui/mod.rs` (`OverlayRequest::{CastPicker, CastPair(DeviceId), CastFirewall, CastOffer(..)}`, `SettingsPage::Cast`), `src/ui/overlays/mod.rs` (mods + `build` arms, placeholder overlays), `src/player/mod.rs` (pub: `CastTo`, `StopCasting`, `snapshot.cast`, `PlayerErrorKind::Cast`, `ControllerDeps.cast`), `src/player/remote.rs` (contract only), `src/cast/mod.rs`, `src/cast/receiver.rs`, `src/cast/relay/mod.rs` (API), `src/cast/airplay/mod.rs` (module list + `Client` API), `src/cast/firewall/mod.rs` (API), `src/bin/tuner-firewall.rs` (stub: exit 2), `src/keymap.rs` (`Action::{CastMenu, StopCasting}` variants only) | — (starts from `main` after `fix/agent-play-placement` merges) | ≈ 0.7k | Opus 5.5 · high |
| **CAST-1** | **AirPlay crypto and codecs:** TLV8, SRP-6a (client and the server half), HAP frame cipher (lenient inbound, caps), pair-setup in both variants (M4/M6 checks, error TLVs, unknown TLVs) and pair-verify, bplist (capped), TXT/`/info` decode incl. versions; the pyatv golden vectors; the scrubbed LAN fixtures; the MIT notice | `src/cast/airplay/{tlv8,srp,hap,pair,bplist,features}.rs`, `scripts/cast/pyatv_vectors.py`, `tests/fixtures/cast/{vectors/*,info_*.plist,txt_*.txt}`, `THIRD_PARTY_NOTICES.md` | CAST-0 | ≈ 1.7k | Opus 5.5 · high |
| **CAST-2** | **AirPlay session:** HTTP/RTSP mux (one in flight, CSeq, stray-reply guard), SETUP (PTP), event channel (keys, acks, reassembly, decode), RECORD/`skipRecord`, `/info`, type 130, `/command` builders per profile, replace, stop, feedback, position (`playbackInfo` + estimate), third-party profile, dev trace, the `Receiver` impl | `src/cast/airplay/{rtsp,session,queue,events,position,trace}.rs`, `src/cast/airplay/mod.rs` (body) | CAST-0, CAST-1 (API) | ≈ 1.8k | Opus 5.5 · xhigh |
| **CAST-3** | **Fake receiver and AirPlay tests:** the §14.3 fake with every mode; `tests/cast_airplay.rs` (§14.4, each scenario × V26/V27); the interop script and its ignored tests (run once) | `tests/support/fake_airplay.rs`, `tests/cast_airplay.rs`, `scripts/cast/pyatv_interop.py` | CAST-0, CAST-1 (vectors, server half); scenarios go green when CAST-2 lands | ≈ 1.6k | Opus 5.5 · high |
| **CAST-4** | **Relay and media pipeline:** upstream proxy (head buffer, single connection, variant pick, statuses), probe, `plan` (interlaced → L3 for AirPlay), ffmpeg supervisor (PDEATHSIG, progress, `CpuTooSlow`), playlist guard + IDR check + playlist views, LAN server, tokens, test-card input | `src/cast/relay/{server,token,upstream,probe,plan,ffmpeg,playlist}.rs`, `tests/cast_relay.rs`, `tests/fixtures/cast/ts/*` | CAST-0 | ≈ 2.0k | Opus 5.5 · high |
| **CAST-5** | **Controller remote output:** `Output`, `CastTo`/`StopCasting`, `load_now_by` branch, lease on `UpstreamClosed`, the command mapping, the event adapter (incl. `StoppedOnTv`), mpv hint read, remote timeouts, route refusals | `src/player/controller.rs`, `src/player/remote.rs` (adapter body), `src/player/state.rs` (timeouts; sign-off), `src/player/mpv.rs` (hint read), `src/playback/route.rs` (+ tests in those modules) | CAST-0 | ≈ 0.8k | Opus 5.5 · high |
| **CAST-6** | **Cast service, discovery, session, lifecycle:** `CastService`/`CastHandle` (incl. `firewall()`, `firewall_job()`), mdns-sd discovery + Null/Fake, registry (versions), the session orchestrator (receiver + relay, escalation, fetch watchdog → `Firewall` via detection, `test_on_tv`), logind inhibitor, store access, Services wiring (start, `apply_config`, shutdown incl. a pending `pkexec`, `exit_cleanup`), `AppShared::set_cast` | `src/cast/{service,session,discovery,registry,inhibit}.rs`, `src/store/cast.rs`, `src/services.rs`, `src/shared.rs`, `tests/cast_service.rs` | CAST-0 | ≈ 1.5k | Opus 5.5 · high |
| **CAST-7** | **Firewall:** detection (files, `systemctl`, interface and subnet), the `user.rules` parser (shadowing, tagged rules), the Allow/Remove runner (both argv forms, outcomes, timeout, cancel), the root helper with validation, the polkit policy, the copy texts, fixtures and tests (§14.7) | `src/cast/firewall/{mod,ufw,allow}.rs` (mod.rs body), `src/bin/tuner-firewall.rs`, `packaging/polkit/io.github.rthoms334.omatuner.firewall.policy`, `tests/cast_firewall.rs`, `tests/fixtures/cast/ufw/*` | CAST-0 | ≈ 0.9k | Sonnet 5.5 · high |
| **CAST-8** | **Player UI and keys:** OSD `Control::Cast` (all exhaustive matches), player menu items, casting card + error buttons (Allow on firewall included), mini-player glyph, banner flashes, picker overlay, PIN overlay (Show a new code, backoff and expiry copy), glyphs, key tables + `feature_on`, live-preview autoplay guard | `src/ui/screens/player/{osd,mod,banner,cards}.rs`, `src/ui/video_host.rs`, `src/ui/overlays/{cast_picker,cast_pair}.rs`, `src/ui/widgets/glyphs.rs`, `src/keymap.rs` (tables), `src/ui/screens/live.rs` + `guide.rs` (one-line guard each) | CAST-0 | ≈ 1.1k | Sonnet 5.5 · high |
| **CAST-9** | **Setup surfaces, app wiring and off-screen alerts:** first-run pass, card and wizard (pair → network with Allow → test), firewall card (Allow, waiting, outcomes, Show command), Settings ▸ Cast page (+ `PAGES_ALL`, `Feature::Cast` arms; firewall row with Allow/Remove; versions). In `app.rs`: `start_cast_pass`, card gate + single-card queue, status chip, `global_action`/player action dispatch, pop-in on cast start, pop-out refusal, the quit confirmation raising the window, the §3.8 alert routing (`self_shown`), the notification's Show → raise. Also Up next's PIN notification while casting | `src/ui/overlays/{cast_offer,cast_firewall}.rs`, `src/ui/screens/settings.rs`, `src/ui/screens/settings/cast.rs`, `src/app.rs`, `src/platform/notify.rs` (one added function), `src/queue/service.rs` (one condition), `tests/cast_app.rs` | CAST-0 | ≈ 1.3k | Opus 5.5 · medium |
| **CAST-10** | **Agent / MCP:** `cast_devices`, `play.device` (in-place placement, PIN raises), `control` actions (+ `popout`/`window` unavailable while casting), `status.casting` (+ `error`), `diagnose` cast section, bridge no-retry and launch rule, instructions; skills `watch`/`kids`/`fix` lines | `src/agent/{proto,tools,server,mcp,mod,act,health}.rs`, `plugins/omatuner/**`, tests in `tests/agent_app.rs` (appended section only) | CAST-0 | ≈ 0.6k | Sonnet 5.5 · high |
| **CAST-11** | **Docs and packaging (integrator, last):** DESIGN §40 + §11.5/§13.8/§15.2 (`keymap_md`)/§20 (LAN listener, root helper, pkexec)/§21 (cast notifications)/§22 (helper, policy, notices)/§30 (casting, §30.13 placement); README; `docs/MANUAL_CHECKLIST.md` (§18); PKGBUILD (`ffmpeg`, `optdepends` polkit, helper, policy, notices); `omatuner.install` hint | `docs/DESIGN.md`, `README.md`, `docs/MANUAL_CHECKLIST.md`, `packaging/PKGBUILD`, `packaging/omatuner.install` | CAST-1…10 | ≈ 0.7k | Sonnet 5.5 · medium |
| **GATE** | **One combined gate, review and fix** (§15.0 step 4); the pyatv interop run again; then the user's real-TV checklist (§18) | — | all | — | Opus 5.5 · high |

**Overlap notes**
- **Only in CAST-0:** `Cargo.toml`, `config.rs`, `src/player/mod.rs`, `src/ui/mod.rs`, `src/ui/overlays/mod.rs` and the Action enum. After that, the items touch disjoint files.
- **Split files:**
  - `keymap.rs`: CAST-0 adds the variants and CAST-8 owns the tables.
  - `src/cast/airplay/mod.rs` and `src/cast/firewall/mod.rs`: CAST-0 writes the declarations and signatures; CAST-2 and CAST-7 own the bodies.
- **`tests/agent_app.rs`** gets an appended section only.
- **CAST-2 (client) and CAST-3 (fake)** are built in parallel from §7 and §14.3 by different agents, so a misreading shows up as a failing test rather than an agreement. The pyatv vectors anchor both.
- **CAST-6** compiles against the CAST-2, CAST-4 and CAST-7 APIs from the skeleton, and tests with `FakeReceiver` and a stub relay until they land.
- **`src/agent/act.rs` and `src/app.rs`** are also changed by `fix/agent-play-placement` (the §30.13 visible-window rule). CAST-0 starts from `main` after that branch merges, so CAST-9 and CAST-10 build on the new rule.

### 15.2.1 The skeleton as built (CAST-0, reviewed)

The skeleton is commit "Cast skeleton (CAST-0)" on `feat/cast`, based on `main` 32711ee (with `fix/agent-play-placement`). Where it differs from §5.4 and §13, the code is the contract:

- **Names.** `gen` is `generation` everywhere (a reserved word in Rust 2024). `ControllerDeps` is `PlayerDeps.cast`; picture A gets `Some(AppShared)`, which forwards to the published `CastHandle`.
- **Schema.** `cast_device` has a `manual` column. `cast_hint` is keyed `(source_id, kind, item_key, device)` and has `forced` ("Convert this channel").
- **Config.** `checked_version` and `default_device` are `Option<String>`. `enabled` defaults to on; the service starts only when `cast::ENABLED && cast.enabled`.
- **Fakes.** `receiver::Backend::Fake` is `#[doc(hidden)] pub` (for `tests/cast_service.rs`). `cast::testing::FakeCast` is frozen: wrap it in your own `CastApi` impl for a new knob.
- **Added by the review**, so that no two items edit one file:
  - `RemoteEvent::{UpstreamEof, StreamInfo, BehindLive}`; `RelayEvent::Probed`; `Prepared.behind_live_s`.
  - `PlayerEvent::CastEnded { device, name, kind }`: `StoppedOnTv` ends casting without an error state, and the app still alerts (§3.4, §3.8).
  - `PlatformEvent::ShowPlayer`: a cast notification's **Show** (§3.8), with a no-op arm in `app.rs`.
  - `AgentPlay.device`: `play {device}` (§12); CAST-10 sets it, CAST-9 plays it with `CastTo` and skips placement.
  - `RouteState.casting`, filled at every caller (`app/multiview.rs`, `queue/service.rs`, `platform/notify.rs`). CAST-5 writes the refusal in `route()`. CAST-9 uses it in the scheduler so a switch while casting doesn't raise the window.
  - `CastApi::convert_current()` ("Convert this channel"). `firewall_allow()` also moves Tuner's rule when the port changed.
  - `glyphs::{CAST, CAST_CONNECTED}` (U+F0118 and U+F0119, checked with `fc-query`).
  - `airplay::pair::server::{Accessory, PairSetupServer, PairVerifyServer}` signatures, so CAST-3 builds in parallel with CAST-1.
  - `PlayerCommand::CastTo { play: None }` when nothing plays replays the last item. This is **Try again** and **Cast again**.
- **Changed by the review of the integrated branch.** `airplay::Client::connect` takes the receiver's TXT identifiers (`features::ReceiverIds`, kept on `Device` and `Sighting` from discovery) for the third-party `channelID` and `ClockPorts` key (§7.7).
- **Actions.** `C` and the player menu's cast items are handled in `ui/screens/player/mod.rs` (CAST-8). `app.rs` (CAST-9) handles only the status chip, global contexts and the alerts.

**Final file ownership.** Every file has one owner. "Split" means CAST-0 froze the declarations and the named item writes the bodies.

| Item | Files |
|---|---|
| CAST-0 (frozen) | `Cargo.toml`, `Cargo.lock`, `src/lib.rs`, `src/keymap.rs` (the `Action` variants), `src/model/{mod,cast}.rs`, `src/config.rs`, `src/paths.rs`, `src/store/{mod,migrations}.rs`, `src/store/migrations/0014_cast.sql`, `src/ui/mod.rs`, `src/ui/overlays/mod.rs`, `src/player/mod.rs`, `src/platform/mod.rs`, `src/cast/{mod,receiver,testing}.rs`, `docs/design/cast-design.md`. The skeleton also added the field `AgentPlay.device` to `src/agent/mod.rs` and `RouteState.casting` to `src/playback/route.rs`, which stay with CAST-10 and CAST-5. |
| CAST-1 | `src/cast/airplay/{tlv8,srp,hap,pair,bplist,features}.rs`, `scripts/cast/pyatv_vectors.py`, `tests/fixtures/cast/{vectors/*,info_*.plist,txt_*.txt}`, `THIRD_PARTY_NOTICES.md` |
| CAST-2 | `src/cast/airplay/{rtsp,session,queue,events,position,trace}.rs`, `src/cast/airplay/mod.rs` (split) |
| CAST-3 | `tests/support/fake_airplay.rs`, `tests/cast_airplay.rs`, `scripts/cast/pyatv_interop.py` |
| CAST-4 | `src/cast/relay/{server,token,upstream,probe,plan,ffmpeg,playlist}.rs`, `src/cast/relay/mod.rs` (split), `tests/cast_relay.rs`, `tests/fixtures/cast/ts/*` |
| CAST-5 | `src/player/controller.rs`, `src/player/remote.rs` (split: the adapter), `src/player/state.rs`, `src/player/mpv.rs`, `src/player/reconnect.rs`, `src/playback/route.rs` (the refusal in `route()`), `tests/player_mpv.rs` (an appended section, §14.5) |
| CAST-6 | `src/cast/{service,session,discovery,registry,inhibit}.rs`, `src/store/cast.rs`, `src/services.rs`, `src/shared.rs`, `tests/cast_service.rs` |
| CAST-7 | `src/cast/firewall/{ufw,allow}.rs`, `src/cast/firewall/mod.rs` (split), `src/bin/tuner-firewall.rs`, `packaging/polkit/io.github.rthoms334.omatuner.firewall.policy`, `tests/cast_firewall.rs`, `tests/fixtures/cast/ufw/*` |
| CAST-8 | `src/ui/screens/player/{osd,mod,banner,cards}.rs`, `src/ui/video_host.rs`, `src/ui/overlays/{cast_picker,cast_pair}.rs`, `src/ui/widgets/glyphs.rs`, `src/keymap.rs` (split: the tables and `feature_on`), `src/ui/screens/live.rs` and `guide.rs` (one-line guard each) |
| CAST-9 | `src/ui/overlays/{cast_offer,cast_firewall}.rs`, `src/ui/screens/settings.rs`, `src/ui/screens/settings/cast.rs`, `src/app.rs` (including the idle inhibitor while casting and the `ShowPlayer` arm), `src/platform/notify.rs` (the cast notification with Show, and no raise for a switch while casting), `src/queue/service.rs` (the PIN notification condition), `src/app/multiview.rs` (a defensive "Stop casting to use multi-view"), `tests/cast_app.rs` |
| CAST-10 | `src/agent/{proto,tools,server,mcp,mod,act,health}.rs`, `plugins/omatuner/**`, `tests/agent_app.rs` (an appended section) |
| CAST-11 | `docs/DESIGN.md`, `README.md`, `docs/MANUAL_CHECKLIST.md`, `packaging/PKGBUILD`, `packaging/omatuner.install` |

### 15.3 Later phases

**Phase 2: Google Cast and polish (about 3–4 weeks of agent work)**
- **Google Cast client, in-tree.** CASTV2 over `tokio-rustls` with a no-verify verifier and a hand-coded 7-field `CastMessage`, not `rust_cast`: that crate panics in `connect` and hides its socket, so it can't have timeouts [GC B.3]. Details:
  - heartbeat 10/10 s;
  - reconnect after 5 s, doubling up to 300 s;
  - DMR `CC1AD845`, `LOAD` with `hlsSegmentFormat` for fMP4;
  - receiver `SET_VOLUME`;
  - SRV port, not a hard-coded 8009;
  - `ca` bit 0x01 for video [GC B.4];
  - capability profiles per model [GC B.2].
- **L1 range pass-through** for faststart MP4 VOD.
- **Seamless zap and reconnect** (`append_list` + DISCONTINUITY).
- **Catch-up seek** by re-resolving.
- **Audio-track choice** (`-map`).
- **Full-length VOD on the TV:** a complete playlist with the film's real length, every segment made on demand (§8.5: MP4 `stss` and MKV `Cues` read by `Range`, one segment per keyframe run, exact-keyframe restarts; L3 first).
- **Guide and browse "Play on <device>"** menus.
- **Local files and ad-hoc URLs.**
- **Trace replay:** the fake replays the user's real-TV transcripts (§14.3 anchor 4).

**Phase 3: DLNA/UPnP**
- Passive SSDP NOTIFY, plus M-SEARCH from a fixed source port included in the firewall rule.
- SOAP over `reqwest`, descriptions and DIDL-Lite with `quick-xml` (`id="0" parentID="-1" restricted="false"`).
- Poll `GetPositionInfo` every 1 s, because position is never evented [DL corr. 5].
- Live L1 TS with DLNA headers; `rupnp` only if its parser handles the user's TVs.

**Phase 4 (optional)**
- AirPlay seek and volume over MRP or Companion (PR 2919's control-only session).
- Companion-credential pairing fallback.
- Roku `15985`, Kodi, FCast.
- IPv6.
- A PTP follower or an NTP responder, only if declared PTP fails on a tvOS still in use (§16 risk 2).

---

## 16. Risks

| # | Risk | Mitigation |
|---|---|---|
| 1 | **The `/command` flow is reverse-engineered and unmerged in pyatv, and the Apple TV auto-updates** (tvOS 27 shipped 2026-09-14). | The flow is shipping in foxCast (tvOS 27) and VioletRelay (26.5) and implemented in castcli (14,1 on 26.6) [REF]. The client follows the sequence those references share. The 26.x-only extra (`actionAtItemEnd`) is non-fatal. Every unverified tvOS 27 claim is handled tolerantly (§7.6). The fake runs in V26 and V27 modes. A version line goes in the log and `diagnose`. §18 runs on 26.6 and again after the update. `TUNER_CAST_TRACE` turns the real session into fixtures. |
| 2 | **Declared PTP is unverified on 26.6 / AppleTV14,1 and beyond 60 s.** Live TV runs for hours. | It is the timing every long-running video sender uses (PR 2899 measured, foxCast on 27, VioletRelay). Apple's 26.6 notes list no AirPlay changes. §18 item 2 runs 60 minutes with ufw on. If it fails on 26.6 only, the fallback is castcli's NTP-responder flow: about 150 lines plus a UDP rule through the same helper. It is not built now because NTP collapses on 26.5 and 27. |
| 3 | **Pairing flakiness** (470, no PIN, HomeKit room, Allow Access reset on upgrades, "Home only" access). | One connection for pin-start and setup. The HKP 3 → HKP 5 ladder. Error TLVs checked first. A specific hint for each reply. A Pair-again path. Companion pairing in Phase 4. |
| 4 | **Broadcast quirks and CPU.** Interlaced 1080i/576i is now always converted (Apple 1.14), so CPU-only L3 runs for many HD channels. | `-preset veryfast`, frame rate kept (single-rate yadif), `CpuTooSlow` surfaced, and "Send as is" as the opt-out. §18 item 5 measures speed on this M2. The playlist guard, a one-step L2→L3 escalation cached per item and TV, and "Convert this channel" remain. |
| 5 | **Firewall: "nothing happens".** | Static ufw check, a 10 s end-to-end fetch watchdog, a fixed port, **Allow on firewall**, the copy command, and Test on TV in setup. |
| 6 | **A root helper and a GUI password prompt.** | A std-only helper of about 150 lines: fixed verbs, validated arguments, one fixed `ufw` command, no shell, refusing unless it runs under pkexec. polkit authenticates every time (keep 5 min). Tuner never sees the password. Tests use a fake `pkexec`. From a checkout, the generic action shows the full `ufw` command line. |
| 7 | **First LAN-facing listener.** | Narrow bind, listening only during a session, 128-bit redacted tokens per generation, peer check, uniform 404s, one route family, no redirects, connection caps, tests. |
| 8 | **One-connection accounts.** | Lease reuse, stop-before-start, head buffer for the probe, single-variant HLS, the shared race window; tested with `set_session_limit(1)`. |
| 9 | **Latency of 10–20 s and zaps of 6–12 s feel slow.** | Stated in the UI; zaps coalesce; replace on the same session; seamless zap in Phase 2. |
| 10 | **Position on tvOS.** Proven on 2026-10-13: tvOS 27.0 answers `playbackInfo` under `info` with CMTimes, and sends `timeJumped` with a position at every start and seek (§7.3). | Both forms are read. The estimate fallback starts from `timeJumped` or the first fetch after a jump, errs 2 s behind, and is bounded by the fetches since the last jump. §18 item 7 checks both. |
| 11 | **Suspend or a network change kills the relay.** | logind sleep inhibitor; `Lost`/`Unreachable` with Play here; the firewall rule re-offered for a new network. |
| 12 | **Third-party AirPlay TVs (Hisense, VIDAA) may not support play-queue video.** | VioletRelay's profile, checked on the Hisense (§18 item 14). If it can't play there, third-party TVs are no longer listed until it can. Cast or DLNA via those TVs in Phase 2/3. |
| 13 | **Frozen-file churn, and the parallel `fix/agent-play-placement` branch** (`act.rs`, `app.rs`). | One skeleton tag up front, cut after that branch merges; `msg.rs` and `tests/support/mod.rs` untouched. |
| 14 | **Licence mix in the references.** | Only pyatv (MIT) code is ported, with the notice installed. LGPL and GPL projects, and those without a licence, are read for facts only (§7.0). |

---

## 17. Decided

These were the open decisions of revision 1, plus the user's decisions for revision 2.

1. **Native Rust AirPlay. No Python helper**, not even as a fallback. (user)
2. **No hardware spike and no blind build.** Phase 1 is ported from proven implementations (§7.0), tested against a fake derived from theirs (§14.3), and checked on the user's TV at the end (§18). (user)
3. **tvOS auto-updates stay on.** tvOS 26 and 27 are supported from the start, in one code path (§7.6). (user; replaces "pause updates")
4. **Firewall:**
   - an in-app **Allow on firewall** button through `pkexec` and polkit, with a small validated helper;
   - **Remove rule**;
   - the copyable command as fallback;
   - detection without root;
   - the 10 s fetch check.
   
   The rule is the interface plus subnet, TCP 47800 only. (user; scope from revision 1)
5. **Casting moves the stream**; there is no "play on both". One-stream accounts require it.
6. **Converting is "Automatically (uses the CPU)" by default.** Otherwise MPEG-2 and interlaced channels can't be cast.
7. **Surround is in Phase 1** (the user never approved deferring it): multichannel sound on the Apple TV comes as a surround rendition beside the required stereo AAC one (§8.4, verified against Apple's authoring spec 2.3, 2.5, 9.6 and its rendition-group appendix). AC-3, E-AC-3 and AAC 5.1 are copied; anything else becomes AC-3 5.1. If the TV doesn't play it, the session goes on in stereo. Third-party AirPlay TVs stay stereo. (user)
8. **Interlaced is "Auto", which deinterlaces for AirPlay** (Apple rule 1.14), with "Send as is" as an opt-out.
9. **Third-party AirPlay TVs** are listed and offered like the Apple TV, cast with the third-party profile (§7.7). There is no experimental tier: "We either support it or we don't." (user, after the first LAN test)
10. **Startup discovery for the first-run card:** one 5 s browse per Tuner version.
11. **Power while casting:** block system sleep (logind inhibitor, default on), but let lid close suspend as usual.
12. **Claude and casting:** `play { device }` in Phase 1. An agent may start a cast without asking when the user named the TV in that request, or asked for "the TV" while exactly one TV is paired (unpaired ones never make "the TV" ambiguous); with two or more paired, it asks which (§12). (user)
13. **Stop ends the cast.** The TV stays first in the picker, so recasting is `C` then Enter.
14. **About 10–20 s behind live is accepted.** It's inherent to HLS on AirPlay. Phase 2 tries to shave it.
15. **After AirPlay: Google Cast (Phase 2), then DLNA (Phase 3).**
16. **Key `C`** for "Cast to a TV…".
17. **Architect sign-offs in one skeleton tag:**
    - `Cargo.toml`: `mdns-sd =0.21.4`, `num-bigint`, `axum` promotion, direct `aws-lc-rs`/`zbus`/`libc`/`if-addrs`, tokio `process`, the `tuner-firewall` bin;
    - `config.rs` `[cast]`; `paths.rs`; migration 0014; `ui/mod.rs` variants; `player` pub items; the `state.rs` timeouts.
18. **Timing: declared PTP only**; no NTP responder in Phase 1 (§7.3). (references)
19. **Casting never moves Tuner's window**, and alerts follow the user across workspaces (§3.8). (user)
20. **Model policy:** Sonnet 5.5 for routine work, Opus 5.5 for design and hard reasoning, as annotated in §15.2. (user)

---

## 18. Manual checklist on your TV (at the end of Phase 1)

Run these against "Living Room" (192.168.1.5) once the gate has passed.
- Run the **first session with `TUNER_CAST_TRACE=1`**, and keep the transcript for the fixture follow-up.
- Note the tvOS version shown in Settings ▸ Cast.
- **The Apple TV auto-updates.** When it reaches tvOS 27 (or a new 26.x), repeat items 1, 2 and 7.
- The Hisense repeats items 1, 2 and 7 (item 14).

1. **Pairing.**
   - A PIN appears with HKP 3. If it doesn't, does **Show a new code** (HKP 5) work?
   - Pairing completes without a 470.
   - The credentials survive an Apple TV reboot.
   - Note the Allow Access setting and the HomeKit room. Check that "Everyone in your Home" gives the right hint.
2. **Session.**
   - Live TV plays for **60 minutes** with ufw on and only the TCP rule, and events keep arriving (declared PTP).
   - From the trace: does SETUP return `skipRecord`, and what does RECORD answer?
   - `playbackInfo` answers under `info` with CMTimes (2026-10-13, tvOS 27.0): the log never says "playbackInfo goes unanswered".
3. **Firewall button.**
   - With ufw blocking: the cast fails within about 10 s with the Firewall message.
   - **Allow on firewall** shows Omarchy's dialog. Note its text.
   - Cancel leaves everything as it was.
   - Allow adds `allow tcp 47800 0.0.0.0/0 any 192.168.1.0/24 in_wlan0 comment=6f6d…` to `/etc/ufw/user.rules`, and Check again plays the test card.
   - **Remove rule** removes it, with no second prompt within 5 minutes.
   - **Show command** text works when pasted into a terminal.
4. **Live HLS** (TS segments, H.264 + AAC) on a real channel:
   - tune-in time with `min_segments = 2`;
   - delay behind live;
   - behaviour on a long-GOP channel.
5. **Formats.**
   - 1080i25 and 576i deinterlaced (L3): no combing, and ffmpeg's `speed` stays above 1.0× on this M2.
   - MP2 converted to AAC.
   - **Surround** (§8.4): a film with AAC 5.1 (the 2026-10-13 one, `xt:vod:346970`), a channel with AC-3 5.1 and a film with E-AC-3. The log says `… aac 5.1(side) 48 kHz + surround (copied)`; the TV's audio menu shows one track; an AVR (or the Apple TV's Settings ▸ Video and Audio ▸ Audio Format) shows 5.1. With the TV on stereo speakers, the sound is the stereo group. If the TV doesn't start it, the log says "again in stereo" and the cast goes on.
   - HEVC (and 10-bit) in fMP4.
6. **Segment hygiene on real channels.**
   - `ffprobe -show_frames` on each segment's first frame: is it an IDR?
   - Does the playlist guard hold a segment back ("a segment outgrew the target duration: restarting with a longer one")? Never twice for one film. TARGETDURATION never changes within a playlist, nor between the loads of one item.
7. **Controls.**
   - Pause with `setRate 0` and resume, including after more than 80 s paused.
   - Seek inside the produced part (playlist view) and beyond it (`-ss`): does the TV land at the right time?
   - Zap by replace: the new channel appears with no stuck old picture.
   - Stop really stops the TV.
   - Stopping from the Apple TV remote gives "Living Room stopped playing".
   - For VOD, `itemPlayedToEnd` arrives, Up next continues on the TV, and the progress shown matches the TV.
8. **VOD EVENT playlist:** the TV starts before `ENDLIST`, at the resume point (not 10 s later); resume a film at 1:10:00. Pause the TV for 5 minutes: the log says "pausing production … ahead of the TV", and playback resumes cleanly. The TV's scrubber shows about a minute ahead, not the whole film.
   - **A reload keeps the place:** when the provider drops a film's connection, the log says "resuming the provider's stream at byte …" and the TV doesn't jump. Convert this channel 10 minutes into a film resumes within a few seconds of where the TV was, never at the cast's start; so does a reconnect.
   - The session's end is in the log at INFO with why (Play here, Stop casting, stopped on the TV…).
9. **Stray requests:** does the TV fetch anything besides the playlist and segments (`/favicon.ico`, `HEAD`, `Range`)? Check that the 404s are harmless.
10. **Volume:** confirm there's no working path (expected), so the "TV remote" copy is right.
11. **Provider limit:** with a one-stream account, a relay reconnect inside 45 s of closing gets 403s that the race window absorbs.
12. **Power:** with the logind inhibitor held, the laptop screen locks but casting continues for 30 minutes.
13. **Workspaces (§3.8).**
    - Ask Claude to put a channel on the TV while Tuner is on another workspace. Nothing pops out, raises or moves.
    - A locked channel raises Tuner with the PIN prompt.
    - Block the firewall and cast, then switch workspace. A desktop notification appears, and its **Show** raises Tuner on the casting card with Allow on firewall.
    - Start a cast while the pop-out is up. The pop-out goes back into the window without taking focus.
14. **Hisense.** It is listed in the picker beside the Apple TV. Items 1, 2 and 7 through the third-party profile. If it can't play, fix the profile or stop listing third-party TVs (§7.7).

---

## Appendix A. How the three proposals were judged

The three proposals were scored on five criteria: correctness against the fact-checked research and the code, risk, effort, UX, and fit with Tuner's conventions.

### Native-first: 6.5 / 10

**Strengths**
- The correct seam: a remote output of A's controller, with A's lease reused.
- The most thorough security list.
- It saw that ffmpeg copy mode can break Apple rule 7.4 (IDR at segment start).
- In-tree crypto on aws-lc-rs.

**Weaknesses**
- It hand-builds a TS demuxer, SPS parser and HLS segmenter, about 12–15 engineer-weeks for Phase 1. That duplicates ffmpeg, which is already installed, and Phase 1 *still* needs ffmpeg to transcode.
- It refuses HEVC, MKV, MP2 and LATM in Phase 1, though MP2 is common on European SD channels.
- A separate pairings file instead of the established 0600 DB, which `backup.rs` already excludes.
- A `stop_tv_on_quit` setting is moot: the relay lives inside Tuner, so the TV can't continue anyway.
- It missed the multi-variant HLS connection risk and live preview autoplay.

**Taken:** the seam, the IDR concern (now the playlist guard), `skipRecord` and the frames over 1024 B (revision 2: both handled tolerantly, as neither has public evidence for video), uniform 404s and the peer check, the token rules, and the typed error kinds.

### Pragmatic (pyatv helper): 6 / 10

**Strengths**
- A clear Phase 0 gate (revision 2: replaced by porting from proven implementations, §15.0).
- **A loopback upstream proxy with a head buffer**, so probing costs no second connection and ffmpeg never sees the provider URL.
- **Test on TV** using lavfi.
- `PR_SET_PDEATHSIG`.
- DB credentials excluded from backup.
- An honest cost table for the helper.

**Weaknesses**
- It ships an unmerged pyatv fork plus vendored stand-ins for packages missing from Arch, as a split package on two architectures. That is fragile and goes against Tuner's Rust-only native stack.
- Credentials pass through a second process.
- A `Holder::Cast` budget change is unneeded, since casting runs in A's controller with multi-view off.
- `rust_cast` was suggested for Phase 2 despite its panic and no-timeout problems.

**Taken:** the proxy and head buffer, Test on TV, PDEATHSIG, the hint fast path from mpv, and the per-session tmpfs directory for live. Revision 1 also kept the helper as a documented fallback; revision 2 drops it (user decision).

### UX-first: 8 / 10

**Strengths**
- The best interaction model: "while casting, anything played goes to the TV", plus the error copy, the first-run flow and the quit confirmation.
- Native AirPlay on already-compiled crypto.
- ffmpeg behind a loopback proxy.
- Per-channel escalation and caching.
- It caught the two-week-rule problem with `mdns-sd`.

**Weaknesses**
- Live segments written to the disk cache, where tmpfs is better.
- Probing by parsing ffmpeg's stderr, which is brittle; ffprobe JSON on the head buffer is used instead.
- A 30-segment window.
- It missed multi-variant HLS and live preview autoplay.

**Taken:** the backbone. This design is UX-first's architecture and flow, corrected and extended with the pieces above.

**Contradictions, and the fact that settled each**

| Question | Decision | The fact that settled it |
|---|---|---|
| Native segmenter or ffmpeg? | ffmpeg | ffmpeg is a dependency of mpv, so it is always installed. HEVC needs fMP4 (Apple 1.5). MP2 is in neither receiver's list. Interlaced content needs a transcoder anyway. |
| Separate holder in the budget? | No | The budget is keyed by `TileId`; casting lives in A's controller; multi-view is off. |
| Pairings in a file or the DB? | DB | The DB is 0600 in a 0700 directory, and `backup.rs` uses an explicit table list (checked). |
| Phase 1 VOD: range relay or HLS remux? | HLS remux; range relay in Phase 2 | AVPlayer's parallel range requests could open several upstream connections on one-connection accounts. ffmpeg's sequential read guarantees one, and MKV needs a remux anyway. |
| Sidecar or native? | Native only | pyatv and three of its dependencies are not in the Arch repos; the crypto is already compiled. The user chose native (revision 2). |

---

## Appendix B. Revision 2 (2026-10-12): what changed

- **Phase 0 removed** (§15.0). The defaults it was going to set now cite sources:
  - timing: declared PTP;
  - step order: RECORD then `/info`;
  - feedback after the commands;
  - `min_segments` 2;
  - 4 s segments;
  - interlaced → deinterlace;
  - start position always 0, through playlists that start where playback starts;
  - surround moves to Phase 2.
  
  The `transcript_*` fixtures and the "the transcript decides" rules are gone.
- **New §7.0 Reference implementations:**
  - pinned sources, a permalink per step, the licence policy (port pyatv's MIT code; facts only from LGPL, GPL and unlicensed projects);
  - what is merged upstream and what isn't.
- **§7.2 pairing:**
  - the HKP 3 → HKP 5 ladder;
  - one TCP connection for pin-start and setup;
  - M4 and M6 verification;
  - error-TLV handling;
  - unknown TLVs;
  - the empty-200 "Home only" case.
- **§7.3 session:** the exact SETUP, item and header contents; replace (VioletRelay); stop; the stray-CSeq guard; `skipRecord` and RECORD tolerance; position via `playbackInfo` with an estimate fallback; the dev trace.
- **New §7.6 tvOS 26 and 27:** one code path, a difference table with sources and fake modes, version detection. **New §7.7:** the third-party profile.
- **§9.4 firewall rewritten:** the Allow on firewall button (`pkexec` + `tuner-firewall` helper + polkit policy; generic `pkexec ufw` from a checkout); outcomes; Remove; another-network handling; the copy fallback; the `omatuner cast` tag; no UDP rule.
- **New §3.8:** when Tuner isn't on a visible workspace (the user's note). Casting never moves the window; alerts go to the card, a toast or a desktop notification with Show. This also covers agent placement, pop-in, Up next's PIN notification and quit.
- **§14.3 Fake receiver** derived from the references, with tvOS 26/27 modes and self-consistency anchors (pyatv golden vectors, interop).
- **Firewall and agent changes:** new firewall tests (§14.7); new agent fields `status.casting.error` and the `diagnose` cast section.
- **Config and errors:** config drops `timing_port`, `airplay_timing` and `audio`; `CastErrorKind::TakenOver` becomes `StoppedOnTv`.
- **§15.2 work items:**
  - no P0;
  - the fake receiver split from the session (CAST-3);
  - the firewall as its own item (CAST-7);
  - the per-item model and effort follow the user's policy;
  - the hardware checks are now the end-of-phase checklist (§18).
- **§16 risks and §17 decisions** updated: §17 is now a "Decided" list. "Pause tvOS updates" is reversed.

---

## Appendix C. Revision 3 (2026-10-13): after the first real-TV test

The first cast of a film to "Living Room" (AppleTV14,1, tvOS 27.0 24J361, hkp3, PTP; this M2) "reset back to the start a few times". The log and two redacted traces showed why, and what else needed changing:
- **Positions (§7.3, §11.2).** The TV answered every `playbackInfo` within 40 ms, under `info` with CMTimes; the client read only `value` and estimated from 0 for the whole session, while the TV had started 12.18 s in (no `EXT-X-START` on a growing playlist). Reloads under a generation started from the last 10 s report, or from the cast's first start before one; the controller's reconnect from its last report. Now: both answer forms, `timeJumped` and fetch runs anchor the estimate (2 s behind), EVENT playlists start at 0, in-session reloads ask the TV, the controller's TV clock gives the reconnect's resume point.
- **Target duration (§8.1.4).** Copied HEVC with 10–12 s GOPs raised the served target twice and was escalated to L3 (700 % CPU) as "irregular"; the loads declared 10, 8 and 4 s. Now: a pin with headroom, kept per item, never changed mid-stream; a longer segment restarts the item where the TV is, at the same level.
- **The provider's drop (§8.1.1).** "error decoding response body" was reqwest's label for the provider hanging up mid-body, not a relay timeout (the client has none). Now the cause is logged, and a film's stream resumes from the byte it reached.
- **Production lead (§8.4).** 4× reading made the TV's scrubber show the whole film loaded. Now a 60 s burst, then real time, and a pause beyond 180 s ahead.
- **Surround (§8.4, §17 decision 7)** moved into Phase 1: demuxed renditions, a stereo AAC group and a surround group, per Apple's authoring spec.
- **Field logs:** INFO lines for session and item ends with why, Test on TV's verdict; the dev trace capped at 10 MiB a file.
- **Third-party AirPlay TVs** are listed and offered like the Apple TV, with no experimental tier (§17 decision 9).
- **"The TV" for an agent** is the one paired TV, when there is exactly one (§12, §17 decision 12).
- The 09:34 "live cast" in that log was Test on TV (the 15 s card); its session end now logs.
