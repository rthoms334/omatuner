#!/usr/bin/env bash
# Generate local test media for player/integration tests (never committed). Owner: WP-07.
# usage: scripts/gen-test-media.sh [outdir] [--live]   (default outdir: target/test-media)
# Requires ffmpeg (libx264 + the native aac encoder). Produces:
#   testsrc.mp4      10 s 640x360 25 fps H.264 + 1 kHz sine AAC (VOD, seek tests)
#   bars.ts          10 s 1280x720 30 fps SMPTE bars MPEG-TS (colour checks: 75% green ≈ 191)
#   hls/index.m3u8   6 x 2 s segments, VOD-type HLS (segment-404 fault injection)
#   long/index.m3u8  60 x 1 s segments, 320x180, VOD-type HLS. Played as VOD for progress tests;
#                    tests/support/media.rs also serves it as a sliding-window LIVE playlist.
#   live/            rolling "live" HLS written by `--live` until killed (manual checks only)
# Tests call this through `tests/support/media.rs::ensure_media`, which runs it once into a
# temporary directory and renames it into place, so concurrent test binaries never see
# half-written files.
set -euo pipefail
out="${1:-target/test-media}"
mkdir -p "$out/hls" "$out/long"
ff() { ffmpeg -nostdin -hide_banner -loglevel error -y "$@"; }
x264=(-c:v libx264 -preset veryfast -pix_fmt yuv420p)

ff -f lavfi -i testsrc2=size=640x360:rate=25 -f lavfi -i sine=frequency=1000:sample_rate=48000 \
   -t 10 "${x264[@]}" -g 50 -c:a aac -shortest "$out/testsrc.mp4"
ff -f lavfi -i smptebars=size=1280x720:rate=30 -f lavfi -i sine=frequency=440 \
   -t 10 "${x264[@]}" -c:a aac -shortest -f mpegts "$out/bars.ts"
ff -f lavfi -i testsrc2=size=640x360:rate=25 -f lavfi -i sine=frequency=880 \
   -t 12 "${x264[@]}" -g 50 -c:a aac -shortest \
   -f hls -hls_time 2 -hls_playlist_type vod -hls_segment_filename "$out/hls/seg%03d.ts" "$out/hls/index.m3u8"
# 1 s GOPs so every segment starts with a keyframe and is exactly 1 s long
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -i sine=frequency=550 \
   -t 60 "${x264[@]}" -g 25 -keyint_min 25 -sc_threshold 0 -c:a aac -shortest \
   -f hls -hls_time 1 -hls_playlist_type vod -hls_segment_filename "$out/long/seg%03d.ts" "$out/long/index.m3u8"

if [[ "${2:-}" == "--live" ]]; then
  mkdir -p "$out/live"
  exec ffmpeg -nostdin -hide_banner -loglevel error -re -f lavfi -i testsrc2=size=640x360:rate=25 \
    -f lavfi -i sine=frequency=660 -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 50 -c:a aac \
    -f hls -hls_time 2 -hls_list_size 6 -hls_flags delete_segments "$out/live/index.m3u8"
fi
echo "test media in $out"
