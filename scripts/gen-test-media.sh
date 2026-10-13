#!/usr/bin/env bash
# Generate local test media for player/integration tests (never committed). Owner: WP-07.
# usage: scripts/gen-test-media.sh [outdir] [--live]   (default outdir: target/test-media)
# Requires ffmpeg (libx264 + the native aac encoder). Produces:
#   testsrc.mp4      10 s 640x360 25 fps H.264 + 1 kHz sine AAC (VOD, seek tests)
#   bars.ts          10 s 1280x720 30 fps SMPTE bars MPEG-TS (colour checks: 75% green ≈ 191)
#   hls/index.m3u8   6 x 2 s segments, VOD-type HLS (segment-404 fault injection)
#   long/index.m3u8  60 x 1 s segments, 320x180, VOD-type HLS. Played as VOD for progress tests;
#                    tests/support/media.rs also serves it as a sliding-window LIVE playlist.
#   surround.ts      10 s 320x180 25 fps H.264 + AC-3 5.1 MPEG-TS (casting's surround rendition)
#   surround.mp4     10 s 320x180 25 fps H.264 + AAC 5.1(side) MP4 (the 2026-10-13 film's sound)
#   surround_eac3.mkv, surround_flac.mkv   the same with E-AC-3 5.1 (copied) and FLAC 5.1 (German:
#                    converted to AC-3)
#   avsync.mkv       40 s 320x180 25 fps H.264 (5 s GOPs, B-frames) + AAC 5.1: at each second k a
#                    white picture and a 1 kHz beep on every channel, both lasting 40 ms x (1 + k mod
#                    23), so a marker names its second (casting's A/V alignment, tests/cast_relay.rs)
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

# six distinct tones, one per channel (FL FR FC LFE SL SR)
six="aevalsrc=sin(440*2*PI*t)|sin(550*2*PI*t)|sin(660*2*PI*t)|0.2*sin(60*2*PI*t)|sin(770*2*PI*t)|sin(880*2*PI*t):c=5.1(side):s=48000"
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -i "$six" \
   -t 10 "${x264[@]}" -g 50 -c:a ac3 -b:a 384k -metadata:s:a:0 language=eng -f mpegts "$out/surround.ts"
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -i "$six" \
   -t 10 "${x264[@]}" -g 50 -c:a aac -b:a 320k -metadata:s:a:0 language=eng "$out/surround.mp4"
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -i "$six" \
   -t 10 "${x264[@]}" -g 50 -c:a eac3 -b:a 384k -metadata:s:a:0 language=eng "$out/surround_eac3.mkv"
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -i "$six" \
   -t 10 "${x264[@]}" -g 50 -c:a flac -metadata:s:a:0 language=ger "$out/surround_flac.mkv"

# the marker film: picture and sound share each marker's start and length
mark='lt(mod(t\,1)\,0.04*(1+mod(floor(t)\,23)))'
beep="if($mark\\,0.8*sin(2*PI*1000*t)\\,0)"
ff -f lavfi -i "color=c=black:s=320x180:r=25,drawbox=c=white:t=fill:enable='$mark'" \
   -f lavfi -i "aevalsrc=$beep|$beep|$beep|$beep|$beep|$beep:c=5.1(side):s=48000" \
   -t 40 "${x264[@]}" -g 125 -keyint_min 125 -sc_threshold 0 -bf 2 -c:a aac -b:a 384k "$out/avsync.mkv"

if [[ "${2:-}" == "--live" ]]; then
  mkdir -p "$out/live"
  exec ffmpeg -nostdin -hide_banner -loglevel error -re -f lavfi -i testsrc2=size=640x360:rate=25 \
    -f lavfi -i sine=frequency=660 -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 50 -c:a aac \
    -f hls -hls_time 2 -hls_list_size 6 -hls_flags delete_segments "$out/live/index.m3u8"
fi
echo "test media in $out"
