#!/usr/bin/env bash
# Regenerate the README screenshots (docs/screenshots/*.png) from the made-up demo profile
# (examples/demo_profile.rs). Developer tool, DESIGN §23.1.
#
# usage: scripts/screenshots.sh [shot...]          (default: every shot listed below)
#
# Needs a Hyprland session (each window floats itself at an exact size, centred on the
# current workspace; don't type while it runs), cargo, curl and ffmpeg. pngquant (else ffmpeg) and oxipng
# shrink the PNGs. Nothing of your own setup reaches the images: every shot runs on a
# fresh copy of the demo profile in target/screenshots/, with a bundled theme
# (TUNER_THEME_DIR) instead of your Omarchy theme.
#
# Video: NASA footage, public domain (NASA Image and Video Library, images.nasa.gov; NASA
# media is not copyrighted): "Earth Views from the International Space Station" (HDEV camera)
# and "Artemis I Launch (Press Site)" (NASA/Sam Lott). Downloaded to target/screenshots/media/
# (sha256-checked, never committed); 8–9.5 s of each loop in the demo channels: Earth on most,
# the launch on those in SECOND_CLIP (examples/demo_profile.rs; Lumen Docs among them, so
# multi-view's two pictures differ). The cuts are silent (a null audio track replaces the
# soundtrack). Without a download a channel plays an ffmpeg test pattern instead.
set -euo pipefail
cd "$(dirname "$0")/.."

out=docs/screenshots
work=target/screenshots
media=$work/media
nasa=https://images-assets.nasa.gov/video
earth_url="$nasa/Earth%20Views%20from%20the%20International%20Space%20Station/Earth%20Views%20from%20the%20International%20Space%20Station~medium.mp4"
earth_sha256=caf71b75b1cd19816c55179384ea41e606d6ecd1088432b1e8e85e566917ce98   # 55 MB, 1280×720
launch_url="$nasa/Artemis%20I%20Launch%202022%20CU%20tracking%20from%20Press%20Site_compressed/Artemis%20I%20Launch%202022%20CU%20tracking%20from%20Press%20Site_compressed~medium.mp4"
launch_sha256=a02fd793f0b33ce3ec213d9c77eba9426329412f7cd33428422494258af6bb72  # 100 MB, 1280×720
dark=$PWD/tests/fixtures/theme/tokyo-night       # Omarchy's default theme
light=$PWD/tests/fixtures/theme/catppuccin-latte
big=1600x1000
hero=1440x900   # the README's top image is shown ~880 px wide: a smaller window keeps it legible
half=950x1000

for tool in cargo ffmpeg hyprctl; do
  command -v "$tool" >/dev/null || { echo "screenshots.sh: needs $tool" >&2; exit 1; }
done
[[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]] || { echo "screenshots.sh: run it inside Hyprland" >&2; exit 1; }

cargo build --release --bin tuner --example demo_profile

# --- video: a segment of each NASA source (16:9, 1280×720, silent), or a test pattern
mkdir -p "$media"
# cut <out> <source name> <url> <sha256> <start s> <length s> <test pattern>
cut() {
  local out=$1 src=$media/$2 url=$3 sha=$4 start=$5 len=$6 pattern=$7
  [[ -s $out ]] && return
  if [[ ! -s $src ]] && command -v curl >/dev/null; then
    curl -fsSL --max-time 600 -o "$src.part" "$url" && mv "$src.part" "$src" || rm -f "$src.part"
  fi
  if [[ -s $src ]] && echo "$sha  $src" | sha256sum -c --quiet -; then
    ffmpeg -nostdin -v error -y -ss "$start" -t "$len" -i "$src" -f lavfi -i anullsrc=r=48000:cl=stereo \
      -map 0:v:0 -map 1:a -vf "scale=1280:720,setsar=1" -shortest \
      -c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p -c:a aac -b:a 32k "$out"
  else
    echo "screenshots.sh: no $2 ($url); using a test pattern" >&2
    ffmpeg -nostdin -v error -y -f lavfi -i "$pattern=size=1280x720:rate=25" -f lavfi -i sine=frequency=440 \
      -t 8 -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest "$out"
  fi
}
# bump a name when its cut changes
clip=$media/demo-nasa-earth-v1.mp4    # 0:49.3–0:57.3, swirling clouds over the limb, between cross-fades
clip2=$media/demo-nasa-launch-v1.mp4  # 0:53–1:02.5, SLS clearing the pad's lightning towers
cut "$clip" earth-views-iss-medium.mp4 "$earth_url" "$earth_sha256" 49.3 8 testsrc2
cut "$clip2" artemis-1-launch-medium.mp4 "$launch_url" "$launch_sha256" 53 9.5 smptehdbars

# --- the demo profile (pristine; every shot runs on a copy)
profile=$work/profile
rm -rf "$profile" "$work/run"
target/release/examples/demo_profile "$profile" --media "$clip" --media2 "$clip2" --silent
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
shot live        "$hero" "$dark"  live     "key:Enter;wait:3" 3
shot guide       "$big"  "$dark"  guide    "key:F;key:Down;key:Down;key:Down;key:Enter;wait:0.5;key:Down;key:Down;key:Right" 2
shot movies      "$big"  "$dark"  movies   "key:Right;key:Left"
shot stack       "$big"  "$dark"  movies   "key:Space;wait:1"
shot detail      "$big"  "$dark"  series   "wait:5;key:Enter;wait:1.5"
shot player      "$big"  "$dark"  live     "play:$PLAY_DOCS;wait:3;key:Enter;move:1400,940;wait:0.5" 1
shot popout      "$big"  "$dark"  live     "play:$PLAY_SPORTS;wait:5;text:o;wait:2;move:240,200" 1
shot live-half   "$half" "$dark"  live     "key:Down;key:Down;key:Down;key:Down;key:Enter;wait:3" 3
shot categories  "$big"  "$dark"  live     "categories:$NORTHWIND_ID;wait:1"
shot help        "$big"  "$dark"  home     "key:F1;text:guide;wait:0.5"
shot settings    "$big"  "$light" settings "wait:0.5"
# v0.2 features
shot upnext      "$big"  "$dark"  home     "key:Alt+6;wait:1"
shot multiview   "$big"  "$dark"  live     "play:$PLAY_DOCS;wait:4;key:Ctrl+M;wait:0.5;text:lumen;wait:0.5;key:Enter;wait:5;move:1400,940;wait:0.5" 1
shot following   "$big"  "$dark"  settings "key:Left;key:Down;key:Down;key:Down;key:Down;key:Down;key:Down;wait:1"
shot reminders   "$big"  "$dark"  settings "key:Left;key:Down;key:Down;key:Down;key:Down;key:Down;wait:1"
# the guide check is debounced 30 s (jobs::guide_check::DEBOUNCE): shoot once it is done
shot guidefix    "$big"  "$dark"  settings "key:Left;key:Down;key:Right;key:Down;key:Down;key:Down;key:Down;key:Enter;wait:34"

# Every regeneration adds the images to git history again: rerun it only when the UI changes.
# pngquant (lossy, 256 colours) roughly halves them; oxipng then recompresses losslessly.
# Without pngquant, ffmpeg (already needed) does a 256-colour pass, kept only if smaller; it
# skips the video-heavy shots (live, live-half, popout), where its dithering shows.
if command -v pngquant >/dev/null && ((${#taken[@]})); then
  pngquant --quality 70-90 --skip-if-larger --force --ext .png "${taken[@]}" || true
elif ((${#taken[@]})); then
  for png in "${taken[@]}"; do
    case $(basename "$png" .png) in live | live-half | popout) continue ;; esac
    tmp=${png%.png}.q.png
    if ffmpeg -loglevel error -y -i "$png" -lavfi \
        "split[a][b];[a]palettegen=max_colors=256:stats_mode=full[p];[b][p]paletteuse=dither=sierra2_4a" \
        "$tmp" && (($(stat -c%s "$tmp") < $(stat -c%s "$png"))); then
      mv "$tmp" "$png"
    else
      rm -f "$tmp"
    fi
  done
fi
if command -v oxipng >/dev/null && ((${#taken[@]})); then
  oxipng -q -o 4 --strip safe "${taken[@]}"
fi
echo "${#taken[@]} screenshot(s) in $out"
