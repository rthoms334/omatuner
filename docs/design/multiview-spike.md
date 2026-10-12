# Multi-view spike: two 4K HEVC pictures (MV-1)

This spike measures what two software-decoded 4K HEVC pictures cost on the user's machine: an M2 under Asahi Linux with 4 performance cores and 4 efficiency cores, no hardware HEVC decoding, and mpv/FFmpeg from Arch. It also checks whether `light_decode_small` (`vd-lavc-skiploopfilter=all` for the small picture) saves enough to ship. DESIGN §35 §4.6 sets the bar at 15 % or more.

**Verdict:** two 4K pictures fit, but `light_decode_small` saves about 10–13 %, which is below the bar. It should ship **off**.

## How it was measured

- **The real engine, headless.** `player::tests::spike_two_uhd_hevc_decodes` (ignored by default) starts picture A with `Player::start`, then picture B with `Player::start_tile` on the shared budget. Both use mpv with `vo=null`, `ao=null` and `hwdec=no`, and loop the clip. CPU is the whole process's utime + stime from `/proc/self/stat` over 20–30 s windows. Memory is `VmRSS` and `RssAnon`.
  ```
  TUNER_SPIKE_MEDIA=<clip> [TUNER_SPIKE_SECS=20] cargo test --lib spike_ -- --ignored --nocapture
  ```
- **Clips (synthetic, made with ffmpeg + libx265 `superfast`, SAO on, keyint 50):**
  - "noise": `testsrc2` with temporal noise, 3840×2160 at 50 fps, about 20 Mbit/s, 12 s. This build's libx265 only does 8-bit, so it is 8-bit; real UHD broadcasts are 10-bit and cost somewhat more.
  - "smooth": `mandelbrot` with light noise, 3840×2160 at 50 fps, about 16 Mbit/s, 10 s.
- **The machine was shared.** Other agents were compiling the whole time (load average 4–17 on 8 cores), so the in-player CPU figures vary from run to run, by up to about ±15 %. To get a figure that load doesn't move, I also counted user-space **instructions retired** by `ffmpeg -f null` decoding the first 4 s of each clip. I counted them with `perf_event_open` (a 40-line Python ctypes script, not committed) and pinned the decode to the P-cores (`taskset -c 4-7`).

## Results

### CPU in the player (noise clip, unpinned)

| Phase | Run 1 (load ~10–13) | Run 2 (load ~15–17) | Run 3 (load ~5–10) |
|---|---|---|---|
| One 4K picture | 1.69 cores | 1.74 | 1.33 |
| Two 4K pictures | 3.30 | 3.31 | 3.02 |
| Two, B with light decoding | 3.17 | 2.94 | 2.89 |
| One, light decoding | 1.21 | 1.56 | 1.21 |
| Two, both pictures' video suspended (hidden workspace) | 0.01 | 0.01 | 0.01 |

- A 4K50 picture costs about **1.3–1.7 cores** in mpv. That is more than DESIGN's estimate of 0.86, because the clip is 50 fps and noisy, and mpv's frame threads also run on the E-cores.
- Two pictures cost about **3.0–3.3 cores of 8**. Neither picture dropped a frame at the decoder in any run.
- With both pictures' video suspended (`vid=no` after 10 s without a rendered frame), the cost drops to **0.01 cores**. Only demux and audio remain.

### `light_decode_small`, measured without the load noise

The decode is pinned to the P-cores and runs over the first 4 s (200 frames).

| Clip | Threads | Instructions, default → skip loop filter | CPU time, default → skip |
|---|---|---|---|
| noise | 1 | 30.29 G → 24.97 G (−17.6 %) | 2.78 s → 2.47 s (−11.3 %) |
| noise | 4 | 30.83 G → 25.55 G (−17.1 %) | 3.09 s → 2.69 s (−12.8 %) |
| smooth | 1 | 43.42 G → 36.24 G (−16.5 %) | 3.98 s → 3.51 s (−11.9 %) |
| smooth | 4 | 44.17 G → 36.87 G (−16.5 %) | 4.30 s → 3.75 s (−13.0 %) |

- Skipping deblocking and SAO removes about 17 % of the instructions but only **11–13 % of the CPU time**. The loop filters are dense SIMD code, so they finish quickly for their instruction count.
- In the player, run 3 (the least loaded) shows 9 % for one picture (1.33 → 1.21 cores) and 0.13 cores for the small picture next to a big one.
- **This is below the 15 % bar, so `light_decode_small` should ship off.** The engine still has the switch, `PlayerCommand::SetLightDecode` (it applies from the next load). It is not worth the softer picture by default. The planner's quality fit (put the small picture on an HD copy when one exists) saves far more: an HD stream costs roughly a quarter of a 4K one.

### Memory (process RSS, noise clip)

| State | VmRSS | RssAnon |
|---|---|---|
| A idle (start) | 55 MiB | 21 MiB |
| A playing 4K | 480–531 MiB | 430–481 MiB |
| B started, idle | +4–6 MiB | +3–6 MiB |
| A and B playing 4K | 922–988 MiB | 873–938 MiB |
| B stopped | 810–831 MiB | 760–782 MiB |
| B stopped, **after `malloc_trim`** | 518–529 MiB | 468–480 MiB |
| Both suspended (`vid=no`), after `malloc_trim` | 282–303 MiB | 231–254 MiB |

- An idle picture B costs **about 5 MiB**, well under DESIGN's 25 MiB. Keeping B alive until exit, instead of recreating it, is cheap.
- A 4K picture's decoder costs about **0.45 GiB**. B's demuxer cache is capped at 64 MiB forward and 8 MiB back (`mpv::create`, `runtime_settings`), so most of that is the decoder's frame pool.
- **`malloc_trim` matters with two decoders.** Stopping B frees about 110 MiB by itself. A trim then gives back another **~290 MiB**. `platform::memory` now watches both pictures and trims after "picture closed", a reload, video off, or a stop.

## What the numbers mean for MV-2 and the defaults

1. Two 4K pictures at 50 fps fit at about 3–3.3 of 8 cores. Recommend side by side or big + small without restrictions.
2. Set `player.multiview.light_decode_small` to **false** by default, or keep the setting and don't send `SetLightDecode`. This is the integrator's call in `config.rs`. Leave the setting in Settings ▸ Player for anyone who wants it.
3. Make quality fit the main CPU saver: when the planner places the small picture, prefer an HD copy over a UHD one.
4. A picture that isn't placed (the Live screen shows only the focused one) or a hidden window costs almost nothing after 10 s. Each picture suspends on its own clock (`player::tests::video_is_suspended_while_nothing_renders_and_resumed_on_request`).
5. Not covered here (it needs a GPU, so it belongs on `MANUAL_CHECKLIST.md`): rendering two textures through the libmpv render API, a 1-hour soak with RSS, and a real UHD channel from each playlist.
