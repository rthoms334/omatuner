#!/usr/bin/env bash
# Regenerate the README screenshots (docs/screenshots/*.png) from the made-up demo profile
# (examples/demo_profile.rs). Developer tool, DESIGN §23.1.
#
# usage: scripts/screenshots.sh [shot...]          (default: every shot listed below)
#
# Needs a Hyprland session (each window floats itself at an exact size, centred on the
# current workspace; don't type while it runs), cargo, curl and ffmpeg. oxipng shrinks the
# PNGs when installed. Nothing of your own setup reaches the images: every shot runs on a
# fresh copy of the demo profile in target/screenshots/, with a bundled theme
# (TUNER_THEME_DIR) instead of your Omarchy theme.
#
# Video: the "Sintel" trailer, © copyright Blender Foundation | durian.blender.org,
# Creative Commons Attribution 3.0 (https://durian.blender.org/sharing/). It is downloaded to
# target/screenshots/media/ (never committed); 8 s of it loop in the demo channels. Without
# a download the channels play an ffmpeg test pattern instead.
set -euo pipefail
cd "$(dirname "$0")/.."

out=docs/screenshots
work=target/screenshots
media=$work/media
clip_url=https://download.blender.org/durian/trailer/sintel_trailer-720p.mp4
clip_sha256=cb0fe73fc0a7d543459996c0cdab730997e6eac1013d3ede18796f777cb7f273
dark=$PWD/tests/fixtures/theme/tokyo-night       # Omarchy's default theme
light=$PWD/tests/fixtures/theme/catppuccin-latte
big=1600x1000
half=950x1000

for tool in cargo ffmpeg hyprctl; do
  command -v "$tool" >/dev/null || { echo "screenshots.sh: needs $tool" >&2; exit 1; }
done
[[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]] || { echo "screenshots.sh: run it inside Hyprland" >&2; exit 1; }

cargo build --release --bin tuner --example demo_profile

# --- video: 8 s of the Sintel trailer (letterbox cropped to 16:9), or a test pattern
mkdir -p "$media"
if [[ ! -s $media/demo.mp4 ]]; then
  src=$media/sintel_trailer-720p.mp4
  if [[ ! -s $src ]] && command -v curl >/dev/null; then
    curl -fsSL --max-time 300 -o "$src.part" "$clip_url" && mv "$src.part" "$src" || rm -f "$src.part"
  fi
  if [[ -s $src ]] && echo "$clip_sha256  $src" | sha256sum -c --quiet -; then
    ffmpeg -nostdin -v error -y -ss 27 -i "$src" -t 8 -vf "crop=968:544:156:88,scale=1280:720,setsar=1" \
      -c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p -c:a aac -b:a 96k "$media/demo.mp4"
  else
    echo "screenshots.sh: no Sintel trailer ($clip_url); using a test pattern" >&2
    ffmpeg -nostdin -v error -y -f lavfi -i testsrc2=size=1280x720:rate=25 -f lavfi -i sine=frequency=440 \
      -t 8 -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest "$media/demo.mp4"
  fi
fi

# --- the demo profile (pristine; every shot runs on a copy)
profile=$work/profile
rm -rf "$profile" "$work/run"
target/release/examples/demo_profile "$profile" --media "$media/demo.mp4" --silent
# NORTHWIND_ID, AURORA_ID, HARBOR_ID, PLAY_DOCS, PLAY_SPORTS, PLAY_NEWS
# shellcheck disable=SC1091
source "$profile/demo/refs.env"

mkdir -p "$out"
wanted=("$@")
taken=()

# shot <name> <WxH> <theme dir> <view> <script> [seconds after the script]
shot() {
  local name=$1 size=$2 theme=$3 view=$4 script=$5 after=${6:-2}
  if ((${#wanted[@]})) && [[ " ${wanted[*]} " != *" $name "* ]]; then return; fi
  local dir=$work/run/$name png=$out/$name.png
  rm -rf "$dir" "$png"
  mkdir -p "$work/run"
  cp -a "$profile" "$dir"
  echo "screenshot: $name ($size)"
  TUNER_THEME_DIR=$theme TUNER_SCREENSHOT=$PWD/$png TUNER_SCREENSHOT_SIZE=$size \
    TUNER_SCREENSHOT_SCRIPT=$script TUNER_SCREENSHOT_AFTER=$after \
    timeout 120 target/release/tuner --profile-dir "$dir" --view "$view" >"$dir/stdout.log" 2>&1 || true
  if [[ -s $png ]]; then
    taken+=("$png")
  else
    echo "screenshot: $name FAILED (see $dir/state/tuner.log)" >&2
  fi
}

# Scripts: steps run 0.35 s apart once the library is loaded (src/devshot.rs).
shot home        "$big"  "$dark"  home     ""
shot live        "$big"  "$dark"  live     "key:Down;key:Down;key:Enter;key:Down;key:Down;wait:3" 3
shot guide       "$big"  "$dark"  guide    "key:Down;key:Down;key:Right" 2
shot movies      "$big"  "$dark"  movies   "key:Tab"
shot stack       "$big"  "$dark"  movies   "key:Tab;key:Space;wait:1"
shot detail      "$big"  "$dark"  movies   "key:Tab;key:Enter;wait:1.5"
shot player      "$big"  "$dark"  live     "play:$PLAY_DOCS;wait:3;key:Enter;move:1400,940;wait:0.5" 1
shot popout      "$big"  "$dark"  live     "play:$PLAY_SPORTS;wait:3;key:O;wait:2" 2
shot live-half   "$half" "$dark"  live     "key:Down;key:Enter;key:Down;wait:3" 3
shot categories  "$big"  "$dark"  live     "categories:$NORTHWIND_ID;wait:1"
shot help        "$big"  "$dark"  home     "key:F1;text:guide;wait:0.5"
shot settings    "$big"  "$light" settings "wait:0.5"

if command -v oxipng >/dev/null && ((${#taken[@]})); then
  oxipng -q -o 4 --strip safe "${taken[@]}"
fi
echo "${#taken[@]} screenshot(s) in $out"
