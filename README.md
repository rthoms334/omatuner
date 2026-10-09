# Tuner

Keyboard-first IPTV player for [Omarchy](https://omarchy.org) (Hyprland): M3U and Xtream
playlists, XMLTV guide with catch-up, movies & series with resume, global search, MPRIS — and
it looks like the rest of your Omarchy desktop (live theme, square corners, JetBrains Mono
Nerd Font).

Status: **skeleton** — the architecture, contracts and work plan are in
[`docs/DESIGN.md`](docs/DESIGN.md). The window launches with the live Omarchy theme; screens
are placeholders until their work packages land.

```sh
cargo run                      # launch (uses ~/.config/tuner, ~/.local/share/tuner)
cargo run -- --profile-dir /tmp/tuner-dev   # isolated profile
cargo test                     # unit + smoke tests (no network/ffmpeg needed)
cargo clippy --all-targets -- -D warnings   # lint gate (zero warnings)
cargo run --example keymap_md  # regenerate the keymap tables for the design doc
scripts/gen-test-media.sh      # ffmpeg test media for player tests
scripts/gen-big-fixtures.py    # 100k-channel M3U + 415k-programme XMLTV for benches
```

For playback on other workspaces, install `packaging/hypr/tuner.lua` (it sets
`render_unfocused`; see DESIGN §11.1).

Requires `mpv` (libmpv ≥ 0.38, client API 2.5), fontconfig, a Wayland session. Licence:
GPL-3.0-or-later.
