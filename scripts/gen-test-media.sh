#!/usr/bin/env bash
# Generate local test media for player/integration tests (never committed).
# usage: scripts/gen-test-media.sh [outdir]   (default: target/test-media)
# Requires ffmpeg. Produces:
#   testsrc.mp4      10 s 640x360 25 fps H.264 + 1 kHz sine AAC (VOD, seek tests)
#   bars.ts          10 s 1280x720 30 fps SMPTE bars MPEG-TS (colour checks: 75% green ≈ 191)
#   hls/index.m3u8   6 x 2 s segments, VOD-type HLS (segment-404 fault injection)
#   live/            rolling "live" HLS written by `--live` until killed (EOF/reconnect tests)
set -euo pipefail
out="${1:-target/test-media}"
mkdir -p "$out/hls"
ff() { ffmpeg -hide_banner -loglevel error -y "$@"; }
ff -f lavfi -i testsrc2=size=640x360:rate=25 -f lavfi -i sine=frequency=1000:sample_rate=48000 \
   -t 10 -c:v libx264 -pix_fmt yuv420p -g 50 -c:a aac -shortest "$out/testsrc.mp4"
ff -f lavfi -i smptebars=size=1280x720:rate=30 -f lavfi -i sine=frequency=440 \
   -t 10 -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest -f mpegts "$out/bars.ts"
ff -f lavfi -i testsrc2=size=640x360:rate=25 -f lavfi -i sine=frequency=880 \
   -t 12 -c:v libx264 -pix_fmt yuv420p -g 50 -c:a aac -shortest \
   -f hls -hls_time 2 -hls_playlist_type vod -hls_segment_filename "$out/hls/seg%03d.ts" "$out/hls/index.m3u8"
if [[ "${2:-}" == "--live" ]]; then
  mkdir -p "$out/live"
  exec ffmpeg -hide_banner -loglevel error -re -f lavfi -i testsrc2=size=640x360:rate=25 \
    -f lavfi -i sine=frequency=660 -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 50 -c:a aac \
    -f hls -hls_time 2 -hls_list_size 6 -hls_flags delete_segments "$out/live/index.m3u8"
fi
echo "test media in $out"
