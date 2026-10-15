#!/usr/bin/env bash
# Generate local test media for player/integration tests (never committed). Owner: WP-07.
# usage: scripts/gen-test-media.sh [outdir] [--live]   (default outdir: target/test-media)
# Requires ffmpeg (libx264 + the native aac encoder). Produces:
#   testsrc.mp4      10 s 640x360 25 fps H.264 + 1 kHz sine AAC (VOD, seek tests)
#   subtitles_none.mkv, subtitles_forced.mkv, subtitles_default.mkv
#                    2 s 160x90 H.264 + English AAC + one English subrip track, flagged as named
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
#   avsync_nocues.mkv  the marker film without cues (written to a pipe): no keyframe index
#   vod.mp4, vod.mkv 30 s 320x180 25 fps H.264 (B-frames, keyframes at irregular times) + AAC
#                    stereo: casting's full-length films (MP4 index, Matroska cues); vod.mp4 has its
#                    moov at the end
#   vod_hevc.mp4     12 s 320x180 HEVC (x265, B-frames, irregular keyframes) + AAC stereo: casting's
#                    fMP4 output of a full-length film
#   vod.ts, vod_nocues.mkv   the same without a keyframe index (MPEG-TS; Matroska written to a
#                    pipe, so without cues): EVENT playlists when cast
#   vod_late.mkv, vod_late.mp4   vod.mp4 with the picture 0.5 s after the sound (a remux of a
#                    broadcast recording: the sound leads), Matroska and MP4 (an empty edit)
#   vod_noelst.mp4   vod.mp4 without an edit list (`-use_editlist 0`): the picture starts one
#                    B-frame delay (0.08 s) after the sound
#   vod_quiet_end.mkv  10 s 320x180 25 fps H.264 (1 s GOPs) + AAC 5.1 that ends at 6 s (silent
#                    credits on a trimmed track): a surround cast's last segments have no sound
#   vod_big.mkv      20 s 320x180 H.264 at a constant 8 Mbit/s (filler) + AAC: 20 MB, more than the
#                    relay's 16 MiB head buffer, so reading its cues is a provider request of its own
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

# Explicit dispositions avoid ffmpeg marking the first subtitle as default automatically.
cat > "$out/.subtitles.srt" <<'EOF'
1
00:00:00,000 --> 00:00:02,000
English subtitle
EOF
for flag in none forced default; do
  disposition="$flag"
  if [[ "$flag" == none ]]; then disposition=0; fi
  ff -f lavfi -i color=c=black:size=160x90:rate=5 -f lavfi -i anullsrc=r=48000:cl=stereo \
     -i "$out/.subtitles.srt" -map 0:v -map 1:a -map 2:s -t 2 "${x264[@]}" -c:a aac -c:s srt \
     -metadata:s:a:0 language=eng -metadata:s:s:0 language=eng -disposition:s:0 "$disposition" "$out/subtitles_$flag.mkv"
done
rm "$out/.subtitles.srt"

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

ffmpeg -nostdin -hide_banner -loglevel error -i "$out/avsync.mkv" -c copy -f matroska - > "$out/avsync_nocues.mkv"

# full-length casting: keyframes where a film's scene cuts would put them
keys=0,1.32,2.68,4.12,5.0,6.6,8.04,9.4,10.8,13.0,14.2,17.6,19.0,21.44,22.0,24.8,27.2,28.6
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -i sine=frequency=330:sample_rate=48000 \
   -t 30 "${x264[@]}" -bf 2 -g 250 -sc_threshold 0 -force_key_frames "$keys" -c:a aac -b:a 128k "$out/vod.mp4"
ff -i "$out/vod.mp4" -c copy "$out/vod.mkv"
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -i sine=frequency=330:sample_rate=48000 \
   -t 12 -c:v libx265 -preset ultrafast -pix_fmt yuv420p -x265-params "bframes=2:keyint=250:scenecut=0:log-level=error" \
   -force_key_frames 0,1.32,2.68,4.12,5.0,6.6,8.04,9.4,10.8 -tag:v hvc1 -c:a aac -b:a 128k "$out/vod_hevc.mp4"
ff -i "$out/vod.mp4" -c copy -f mpegts "$out/vod.ts"
ffmpeg -nostdin -hide_banner -loglevel error -i "$out/vod.mp4" -c copy -f matroska - > "$out/vod_nocues.mkv"
ff -i "$out/vod.mp4" -itsoffset 0.5 -i "$out/vod.mp4" -map 1:v -map 0:a -c copy "$out/vod_late.mkv"
ff -i "$out/vod.mp4" -itsoffset 0.5 -i "$out/vod.mp4" -map 1:v -map 0:a -c copy "$out/vod_late.mp4"
ff -i "$out/vod.mp4" -c copy -use_editlist 0 "$out/vod_noelst.mp4"
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -t 6 -i "$six" \
   -t 10 "${x264[@]}" -g 25 -keyint_min 25 -sc_threshold 0 -c:a aac -b:a 320k -metadata:s:a:0 language=eng "$out/vod_quiet_end.mkv"
ff -f lavfi -i testsrc2=size=320x180:rate=25 -f lavfi -i sine=frequency=330:sample_rate=48000 -t 20 \
   -c:v libx264 -preset ultrafast -pix_fmt yuv420p -b:v 8M -minrate 8M -maxrate 8M -bufsize 2M \
   -x264-params nal-hrd=cbr:force-cfr=1 -g 50 -c:a aac "$out/vod_big.mkv"

if [[ "${2:-}" == "--live" ]]; then
  mkdir -p "$out/live"
  exec ffmpeg -nostdin -hide_banner -loglevel error -re -f lavfi -i testsrc2=size=640x360:rate=25 \
    -f lavfi -i sine=frequency=660 -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 50 -c:a aac \
    -f hls -hls_time 2 -hls_list_size 6 -hls_flags delete_segments "$out/live/index.m3u8"
fi
echo "test media in $out"
