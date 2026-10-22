#!/usr/bin/env bash
# Record the README's demo video from the made-up demo profile (examples/demo_profile.rs).
# Developer tool, DESIGN §23.1.
#
# usage: scripts/demo-video.sh [--assemble] [--check] [--yes] [scene...]
#
#   (no scene)  build, record every scene, then assemble the video
#   scene...    record only these scenes, then assemble from every recording on disk
#   --assemble  record nothing: assemble from the recordings already in target/demo-video/
#   --check     try scenes where you are, without the workspace switch, the question, the
#               countdown or the keyboard focus (a quick look; recordings are kept)
#   --yes       skip the "Record now?" question (the 5 s countdown stays)
#   --reuse     keep the last build and demo profile (the agent's match is on for 90 minutes
#               after the profile is built)
#
# Outputs:
#   target/demo-video/tuner-demo.mp4  H.264, 1440 px wide, under 10 MB: upload it in GitHub's
#                                     README editor (it renders user-attachments videos inline)
#   docs/demo-preview.webp            the README's looping preview (~12 s, 880 px wide)
#
# RECORDING TAKES OVER THE SCREEN for about three minutes: it switches the focused monitor to an
# empty workspace ("tuner-demo"), and every scene opens a floating Tuner window (1280×800,
# centred) over a dark terminal window, and records that part of the screen. Don't type or move
# windows meanwhile, close any shell panels, and turn notifications off (Omarchy: Do Not
# Disturb): whatever is on top of the stage is in the video. Keys and pointer moves reach the
# demo Tuner from inside the app (the devshot driver, src/devshot.rs), never through the
# compositor, so nothing is ever typed into another window; each scene also stops unless the
# focused window is its own demo Tuner. Ctrl+C stops it and returns to your workspace.
#
# Nothing of your own setup is used: every scene runs Tuner on a fresh copy of the demo profile
# in target/demo-video/ with a bundled theme (TUNER_THEME_DIR), and the terminal is a foot
# window with its own config. The capture is grim (wlr-screencopy) at 30 fps through
# scripts/demo-capture.py: gpu-screen-recorder's KMS capture doesn't work on every machine
# (Asahi) and its portal capture asks for a screen in a dialog.
#
# Needs Hyprland, cargo, curl, ffmpeg (libx264, libwebp, drawtext), grim, foot, bat, python3
# and jq. Media (downloaded to target/demo-video/media/, sha256-checked, never committed): the
# public-domain NASA clips on the live channels, and the films' and series' public-domain
# artwork from Wikimedia Commons listed in docs/demo-media.tsv.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD

work=$root/target/demo-video
media=$work/media
runs=$work/run
segs=$work/segments
mp4=$work/tuner-demo.mp4
webp=$root/docs/demo-preview.webp
stage_w=1280 stage_h=800   # the recorded window, logical px
out_w=1440 app_h=900 band=120   # the video: a caption band above the window, scaled to 1440 wide
out_h=$((app_h + band))
fps=30
mp4_budget_kb=9500              # GitHub's free plans take videos up to 10 MB
dark=$root/tests/fixtures/theme/tokyo-night
bg='0x16161e' fg='0xc0caf5' dim='0x7f88b5' accent='0x7aa2f7'   # Tokyo Night

assemble_only=0 assume_yes=0 check=0 reuse=0 wanted=()
for a in "$@"; do
  case $a in
    --assemble) assemble_only=1 ;;
    --yes) assume_yes=1 ;;
    --check) check=1 ;;
    --reuse) reuse=1 ;;
    -h | --help) sed -n '2,42p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "demo-video.sh: unknown option $a" >&2; exit 2 ;;
    *) wanted+=("$a") ;;
  esac
done

need() { for t in "$@"; do command -v "$t" >/dev/null || { echo "demo-video.sh: needs $t" >&2; exit 1; }; done; }
need ffmpeg ffprobe python3
font() { fc-match -f '%{file}' "$1" 2>/dev/null || true; }
# Captions: Adwaita Sans (Inter-based, ships with Omarchy) for the words, the app's own JetBrains
# Mono (fontconfig, as in Tuner) for the small section labels.
sans=${DEMO_FONT:-$(font 'Adwaita Sans:style=Regular')}
mono=${DEMO_MONO_FONT:-$(font 'JetBrainsMono Nerd Font:style=Regular')}
[[ -f $sans ]] || sans=$(font sans-serif)
[[ -f $mono ]] || mono=$(font monospace)

# ------------------------------------------------------------------------------------- scenes
# One line per scene, in the video's order: name|section|caption. The captions are the README's
# feature list (keep both in step). `title` and `end` are the cards.
scenes=(
  "title||"
  "live|Watching|Live TV, with what's on now and next"
  "guide|Watching|A guide for every channel"
  "player|Watching|Controls that stay out of the way"
  "multiview|Watching|Two channels side by side"
  "popout|Watching|Keep watching while you work"
  "movies|Library|Every playlist in one place"
  "series|Library|Series pages, season by season"
  "watchlists|Library|Watchlists for films and series"
  "upnext|Library|Up next lines up what's next"
  "recordings|Downloads & recordings|Record live TV from the guide"
  "downloads|Downloads & recordings|Downloads, grouped by series"
  "agent|AI agents|Ask any AI agent. Tuner puts it on."
  "following|AI agents|Follow teams and shows, with reminders"
  "end||"
)
# The README preview: name|from s|seconds of the assembled segments, looped.
preview=(
  "live|0.2|2.6"
  "multiview|0.4|2.2"
  "movies|1.0|2.4"
  "agent|1.0|5.5"
)

# Each scene's Tuner script (src/devshot.rs): steps 0.35 s apart once the library is in. The
# recording keeps what lies between `mark:in` and `mark:out`; the leading wait lets the recorder
# start. $PLAY_* and playlist ids come from the profile's demo/refs.env.
script_for() {
  case $1 in
    live) echo "wait:2.5;mark:in;wait:0.6;key:Down;wait:0.7;key:Down;wait:0.7;key:Enter;wait:2.4;mark:out" ;;
    guide) echo "wait:2.5;mark:in;wait:0.6;key:Down;wait:0.5;key:Down;wait:0.5;key:Right;wait:0.5;key:Right;wait:0.5;key:Right;wait:1.2;mark:out" ;;
    player) echo "play:$PLAY_DOCS;wait:3;key:Enter;wait:1;mark:in;wait:0.8;move:640,250;wait:1.4;move:700,300;wait:1;move:300,764;wait:1.4;mark:out" ;;
    multiview) echo "play:$PLAY_DOCS;wait:3;key:Enter;wait:1;mark:in;wait:0.6;key:Ctrl+M;wait:0.8;text:lumen;wait:0.8;key:Enter;wait:2.8;mark:out" ;;
    popout) echo "play:$PLAY_SPORTS;wait:3.5;mark:in;wait:0.8;text:o;wait:3.5;mark:out" ;;
    movies) echo "wait:2.5;mark:in;wait:0.6;key:Right;wait:0.5;key:Right;wait:0.5;key:Left;wait:0.3;key:Left;wait:0.6;key:Space;wait:2;mark:out" ;;
    series) echo "wait:3;mark:in;wait:0.6;key:Enter;wait:1.2;key:Down;wait:0.5;key:Down;wait:0.5;key:Down;wait:1.2;mark:out" ;;
    watchlists) echo "key:Alt+7;wait:2.5;mark:in;wait:0.6;key:Down;wait:0.6;key:Enter;wait:1;key:Down;wait:0.5;key:Down;wait:1.4;mark:out" ;;
    upnext) echo "key:Alt+6;wait:2.5;mark:in;wait:0.6;key:Down;wait:0.5;key:Down;wait:1.6;mark:out" ;;
    recordings) echo "wait:2.5;mark:in;wait:0.6;key:Right;wait:0.4;key:Right;wait:0.4;key:Right;wait:0.7;key:Ctrl+D;wait:1;key:Enter;wait:1;key:Alt+8;wait:1.6;mark:out" ;;
    downloads) echo "key:Alt+8;wait:2.5;mark:in;wait:0.8;key:Down;wait:0.4;key:Down;wait:0.4;key:Down;wait:0.7;key:Enter;wait:2;mark:out" ;;
    agent) echo "play:$PLAY_NEWS;wait:3.5;text:o;wait:2;mark:in;wait:0.8;mark:agent;wait:14;mark:out" ;;
    following) echo "wait:2.5;mark:in;wait:0.6;key:Left;key:Down;key:Down;key:Down;key:Down;key:Down;key:Down;wait:1.8;key:Up;wait:1.6;mark:out" ;;
  esac
}
view_for() {
  case $1 in
    live | player | multiview | popout | agent) echo live ;;
    guide | recordings) echo guide ;;
    movies) echo movies ;;
    series) echo series ;;
    following) echo settings ;;
    *) echo home ;;
  esac
}
from_for() { case $1 in following) echo agent ;; *) echo "" ;; esac; }   # continue that scene's profile
term_for() { case $1 in popout) echo code ;; agent) echo agent ;; *) echo plain ;; esac; }
popout_for() { case $1 in popout) echo "512 288" ;; agent) echo "576 324" ;; *) echo "" ;; esac; }

recordable() { [[ $1 != title && $1 != end ]]; }

# ------------------------------------------------------------------------------------ helpers
pids=()
orig_ws=""
cleanup() {
  local p
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  if [[ -n $orig_ws ]]; then goto_workspace "$orig_ws" || true; fi
}

# One window dispatcher: Omarchy 4's Lua syntax first, then classic Hyprland.
dispatch() { # <lua> <classic>
  [[ $(hyprctl dispatch "$1" 2>&1) == ok* ]] || [[ $(hyprctl dispatch $2 2>&1) == ok* ]]
}
goto_workspace() { dispatch "hl.dsp.focus({ workspace = \"$1\" })" "workspace $1"; } # <id or name:…>
client() { hyprctl clients -j | jq -c --argjson pid "$1" 'map(select(.pid == $pid and .mapped)) | first // empty'; }
place() { # <address> <x> <y> <w> <h> (logical, absolute)
  local a=$1
  dispatch "hl.dsp.window.float({ window = \"address:$a\", action = \"enable\" })" "setfloating address:$a" || true
  dispatch "hl.dsp.window.resize({ window = \"address:$a\", x = $4, y = $5 })" "resizewindowpixel exact $4 $5,address:$a"
  dispatch "hl.dsp.window.move({ window = \"address:$a\", x = $2, y = $3 })" "movewindowpixel exact $2 $3,address:$a"
}
# The stage: stage_w×stage_h centred in the focused monitor's usable area (as Tuner's devshot
# centres its own window). Prints "x y mon_x mon_y".
stage_origin() {
  hyprctl monitors -j | jq -r --argjson w "$stage_w" --argjson h "$stage_h" '
    map(select(.focused)) | first |
    (.width / .scale | round) as $mw | (.height / .scale | round) as $mh |
    (.x + .reserved[0]) as $ux | (.y + .reserved[1]) as $uy |
    ($mw - .reserved[0] - .reserved[2]) as $uw | ($mh - .reserved[1] - .reserved[3]) as $uh |
    "\($ux + (($uw - $w) / 2 | floor)) \($uy + (($uh - $h) / 2 | floor)) \(.x) \(.y)"'
}
wait_for() { # <seconds> <command…>: poll until the command succeeds
  local until=$((SECONDS + $1)); shift
  until "$@"; do ((SECONDS < until)) || return 1; sleep 0.1; done
}

foot_ini() {
  cat >"$work/foot.ini" <<EOF
[main]
font=JetBrainsMono Nerd Font:size=14
pad=28x24
[cursor]
blink=no
[colors-dark]
background=1a1b26
foreground=c0caf5
regular0=15161e
regular1=f7768e
regular2=9ece6a
regular3=e0af68
regular4=7aa2f7
regular5=bb9af7
regular6=7dcfff
regular7=a9b1d6
bright0=414868
bright1=f7768e
bright2=9ece6a
bright3=e0af68
bright4=7aa2f7
bright5=bb9af7
bright6=7dcfff
bright7=c0caf5
EOF
  # foot before light/dark themes calls the section [colors]
  foot --check-config -c "$work/foot.ini" >/dev/null 2>&1 || sed -i 's/^\[colors-dark\]$/[colors]/' "$work/foot.ini"
}

# launch <dir> <name> <log> <command…>: start a command through Hyprland (exec_cmd) with a rule
# keeping its window opaque (Omarchy dims every window a little) and, while checking, off the
# keyboard focus. Prints the pid (the command's own: the wrapper execs it).
launch() {
  local dir=$1 tag=$2 log=$3; shift 3
  local wrap=$dir/$tag.sh rules='opacity = "1 1"'
  ((check)) && rules+=', no_initial_focus = true'
  {
    echo '#!/usr/bin/env bash'
    echo "echo \$\$ >'$dir/$tag.pid'"
    echo "cd '$root'"
    printf 'exec'; printf ' %q' "$@"; echo " >'$log' 2>&1"
  } >"$wrap"
  chmod +x "$wrap"
  rm -f "$dir/$tag.pid"
  dispatch "hl.dsp.exec_cmd(\"$wrap\", { $rules })" "exec $wrap" || return 1
  wait_for 10 test -s "$dir/$tag.pid" || return 1
  cat "$dir/$tag.pid"
}
stop_pid() { [[ -n ${1:-} ]] && kill "$1" 2>/dev/null || true; }

# A foot window filling the stage, behind the scene's Tuner: a plain dark backdrop (so nothing
# shows through), a page of Tuner's own source (the pop-out scene) or the agent terminal
# (scripts/demo-agent.py). Prints its pid.
open_term() { # <kind> <scene dir>
  local kind=$1 dir=$2 cmd pid c
  case $kind in
    plain) cmd=(sh -c 'printf "\033[?25l"; exec sleep 3600') ;;
    code) cmd=(sh -c 'printf "\033[?25l"; BAT_CONFIG_PATH=/dev/null bat --paging=never --style=numbers --color=always --theme=ansi --line-range=1:60 "$1"; exec sleep 3600' sh "$root/src/devshot.rs") ;;
    agent) cmd=(python3 "$root/scripts/demo-agent.py" --tuner "$root/target/release/tuner" --profile "$dir/profile" --marks "$dir/marks") ;;
  esac
  pid=$(launch "$dir" term "$dir/term.log" foot -c "$work/foot.ini" --app-id tuner-demo-term --title "Tuner demo" "${cmd[@]}") || return 1
  wait_for 10 eval 'c=$(client '"$pid"'); [[ -n $c ]]' || { echo "demo-video.sh: the terminal never appeared" >&2; stop_pid "$pid"; return 1; }
  read -r sx sy _ _ < <(stage_origin)
  place "$(jq -r .address <<<"$c")" "$sx" "$sy" "$stage_w" "$stage_h"
  echo "$pid"
}

# Record one scene into $runs/<name>/: raw.mp4 (+ raw.mp4.ts, its first frame's clock), marks
# (the script's marks) and last.png (Tuner's own capture of its last frame).
record() {
  local name=$1 dir=$runs/$1 from
  from=$(from_for "$name")
  rm -rf "$dir"
  mkdir -p "$dir"
  if [[ -n $from ]]; then
    [[ -d $runs/$from/profile ]] || { echo "demo-video.sh: $name continues $from: record $from first" >&2; return 1; }
    cp -a "$runs/$from/profile" "$dir/profile"
    rm -rf "$dir/profile/run"
  else
    cp -a "$work/profile" "$dir/profile"
  fi
  local profile=$dir/profile
  # pop-out: a size and place inside the stage (`player.popout_last`, monitor-relative)
  local pop; pop=$(popout_for "$name")
  if [[ -n $pop ]]; then
    read -r pw ph <<<"$pop"
    read -r sx sy mx my < <(stage_origin)
    python3 - "$(find "$profile" -name config.toml -print -quit)" $((sx - mx + stage_w - pw - 24)) $((sy - my + stage_h - ph - 24)) "$pw" "$ph" <<'EOF'
import re, sys
path, x, y, w, h = sys.argv[1], *map(int, sys.argv[2:])
s = open(path).read()
s = re.sub(r"(?m)^popout_last\s*=.*\n", "", s)
s = s.replace("[player]\n", f"[player]\npopout_last = [{x}, {y}, {w}, {h}]\n", 1)
open(path, "w").write(s)
EOF
  fi
  local term tuner c addr cap
  term=$(open_term "$(term_for "$name")" "$dir") || return 1
  pids+=("$term")
  echo "scene: $name"
  tuner=$(launch "$dir" tuner "$dir/tuner.log" env TUNER_THEME_DIR="$dark" TUNER_SCREENSHOT="$dir/last.png" \
    TUNER_SCREENSHOT_SIZE="${stage_w}x${stage_h}" TUNER_SCREENSHOT_SCRIPT="$(script_for "$name")" TUNER_SCREENSHOT_AFTER=0.3 \
    TUNER_SCREENSHOT_MARKS="$dir/marks" "$root/target/release/tuner" --profile-dir "$profile" --view "$(view_for "$name")") || { stop_pid "$term"; return 1; }
  pids+=("$tuner")
  # its window, floated at the stage size by the devshot driver
  if ! wait_for 20 eval 'c=$(client '"$tuner"'); [[ -n $c ]] && [[ $(jq -c .size <<<"$c") == "['"$stage_w,$stage_h"']" ]] && [[ $(jq .floating <<<"$c") == true ]]'; then
    echo "demo-video.sh: $name: Tuner's window never reached ${stage_w}x${stage_h}" >&2; stop_pid "$tuner"; stop_pid "$term"; return 1
  fi
  sleep 0.3
  c=$(client "$tuner")
  addr=$(jq -r .address <<<"$c")
  # above the terminal (a covered window gets no frames, and the script would stall)
  dispatch "hl.dsp.window.alter_zorder({ window = \"address:$addr\", mode = \"top\" })" "alterzorder top,address:$addr" || true
  if ((!check)); then
    # the demo window takes the keyboard; never record unless it is the focused window
    dispatch "hl.dsp.focus({ window = \"address:$addr\" })" "focuswindow address:$addr" || true
    sleep 0.2
    if [[ $(hyprctl activewindow -j | jq -r .pid) != "$tuner" ]]; then
      echo "demo-video.sh: $name: the focused window isn't the demo Tuner; stopping" >&2
      stop_pid "$tuner"; stop_pid "$term"; return 1
    fi
  fi
  read -r x y < <(jq -r '"\(.at[0]) \(.at[1])"' <<<"$c")
  python3 "$root/scripts/demo-capture.py" --region "$x,$y ${stage_w}x${stage_h}" --out "$dir/raw.mp4" --fps "$fps" --width "$out_w" 2>"$dir/capture.log" &
  cap=$!
  pids+=("$cap")
  local until=$((SECONDS + 120))
  while kill -0 "$tuner" 2>/dev/null && ((SECONDS < until)); do sleep 0.1; done
  stop_pid "$tuner"
  kill -INT "$cap" 2>/dev/null || true
  wait "$cap" || true
  stop_pid "$term"
  cat "$dir/capture.log" >&2
  [[ -s $dir/raw.mp4 && -s $dir/raw.mp4.ts && -s $dir/marks ]] || { echo "demo-video.sh: $name: no recording (see $dir/*.log)" >&2; return 1; }
}

# Cut <scene>'s recording to its marks, 1440×900 at $fps: $runs/<name>/cut.mp4.
cut_scene() {
  local dir=$runs/$1 span
  span=$(python3 - "$dir/raw.mp4.ts" "$dir/marks" <<'EOF'
import sys
first = int(open(sys.argv[1]).read().split()[-1])          # realtime µs of the first frame
marks = dict(l.split()[:2] for l in open(sys.argv[2]) if l.strip())
start, end = (int(marks[k]) - first for k in ("in", "out"))
if start < 0:
    sys.exit(f"the recorder started {-start / 1e6:.2f} s after mark:in (lengthen the scene's first wait)")
print(f"{start / 1e6:.3f} {(end - start) / 1e6:.3f}")
EOF
  ) || { echo "demo-video.sh: $1: $span" >&2; return 1; }
  read -r ss len <<<"$span"
  ffmpeg -nostdin -v error -y -ss "$ss" -t "$len" -i "$dir/raw.mp4" \
    -vf "scale=$out_w:$app_h:flags=lanczos,setsar=1" -an -c:v libx264 -preset veryfast -crf 12 -pix_fmt yuv420p "$dir/cut.mp4"
}

# ----------------------------------------------------------------------------------- assembly
# drawtext alpha: in from `at` over 0.35 s; `y` slides up 10 px meanwhile.
fade_in() { echo "if(lt(t,$1),0,if(lt(t,$1+0.35),(t-$1)/0.35,1))"; }
slide() { echo "$1+10*max(0,1-(t-$2)/0.35)"; }

# A title card: <out> <seconds> <title> <line 1> <line 2>
card() {
  local out=$1 len=$2 tdir; tdir=$(mktemp -d "$work/card.XXXX")
  printf '%s' "$3" >"$tdir/t"; printf '%s' "$4" >"$tdir/a"; printf '%s' "$5" >"$tdir/b"
  ffmpeg -nostdin -v error -y -f lavfi -i "gradients=s=${out_w}x${out_h}:r=$fps:d=$len:c0=0x1a1b26:c1=0x101018:x0=0:y0=0:x1=$out_w:y1=$out_h:speed=0.004" \
    -vf "drawtext=fontfile=$sans:textfile=$tdir/t:fontsize=132:fontcolor=$fg:borderw=2:bordercolor=$fg:x=(w-text_w)/2:y='$(slide 'h/2-150' 0.2)':alpha='$(fade_in 0.2)',
         drawtext=fontfile=$sans:textfile=$tdir/a:fontsize=40:fontcolor=$fg:x=(w-text_w)/2:y='$(slide 'h/2+30' 0.6)':alpha='$(fade_in 0.6)',
         drawtext=fontfile=$mono:textfile=$tdir/b:fontsize=24:fontcolor=$dim:x=(w-text_w)/2:y='$(slide 'h/2+100' 0.9)':alpha='$(fade_in 0.9)',
         fade=t=out:st=$(python3 -c "print($len-0.45)"):d=0.45:color=$bg,format=yuv420p" \
    -c:v libx264 -preset veryfast -crf 12 "$out"
  rm -rf "$tdir"
}

# A feature: the cut recording under a caption band. <out> <scene> <section> <caption>
feature() {
  local out=$1 dir=$runs/$2 tdir; tdir=$(mktemp -d "$work/cap.XXXX")
  printf '%s' "${3^^}" >"$tdir/s"; printf '%s' "$4" >"$tdir/c"
  ffmpeg -nostdin -v error -y -i "$dir/cut.mp4" \
    -filter_complex "[0:v]pad=$out_w:$out_h:0:$band:color=$bg,
      drawtext=fontfile=$mono:textfile=$tdir/s:fontsize=20:fontcolor=$accent:x=(w-text_w)/2:y='$(slide 24 0.05)':alpha='$(fade_in 0.05)',
      drawtext=fontfile=$sans:textfile=$tdir/c:fontsize=42:fontcolor=$fg:borderw=1:bordercolor=$fg:x=(w-text_w)/2:y='$(slide 54 0.15)':alpha='$(fade_in 0.15)',
      format=yuv420p[v]" -map "[v]" -c:v libx264 -preset veryfast -crf 12 "$out"
  rm -rf "$tdir"
}

assemble() {
  rm -rf "$segs" && mkdir -p "$segs"
  local list=$segs/list.txt line name section caption n=0
  : >"$list"
  for line in "${scenes[@]}"; do
    IFS='|' read -r name section caption <<<"$line"
    local seg; seg=$segs/$(printf '%02d' $n)-$name.mp4
    case $name in
      title) card "$seg" 3.2 "Tuner" "Live TV, films and series from all your playlists" "an IPTV player for Omarchy" ;;
      end) card "$seg" 3.6 "Tuner" "Free and open source" "github.com/rthoms334/omatuner" ;;
      *)
        if [[ ! -s $runs/$name/cut.mp4 ]]; then echo "assemble: $name not recorded, left out" >&2; continue; fi
        feature "$seg" "$name" "$section" "$caption" ;;
    esac
    echo "file '$seg'" >>"$list"
    n=$((n + 1))
  done
  local whole=$segs/whole.mp4
  ffmpeg -nostdin -v error -y -f concat -safe 0 -i "$list" -c copy "$whole"
  local secs; secs=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$whole")
  # two-pass x264 to fit the upload limit: bitrate = budget / length
  local kbps; kbps=$(python3 -c "print(int($mp4_budget_kb * 8 / $secs * 0.97))")
  echo "assemble: $(printf '%.1f' "$secs") s, ${kbps} kb/s"
  (cd "$segs" && ffmpeg -nostdin -v error -y -i "$whole" -c:v libx264 -preset slow -b:v "${kbps}k" -pass 1 -an -f null /dev/null &&
    ffmpeg -nostdin -v error -y -i "$whole" -c:v libx264 -preset slow -b:v "${kbps}k" -maxrate "$((kbps * 3))k" -bufsize "$((kbps * 4))k" \
      -pass 2 -pix_fmt yuv420p -movflags +faststart -an "$mp4")
  # the preview: a few moments of the features, captions and all
  local inputs=() filters="" i=0 item
  for item in "${preview[@]}"; do
    IFS='|' read -r name from secs <<<"$item"
    local seg; seg=$(ls "$segs"/*-"$name".mp4 2>/dev/null | head -1)
    [[ -n $seg ]] || { echo "assemble: preview: $name missing" >&2; continue; }
    inputs+=(-ss "$from" -t "$secs" -i "$seg")
    filters+="[$i:v]setpts=PTS-STARTPTS,fps=12,scale=880:-2:flags=lanczos[p$i];"
    i=$((i + 1))
  done
  if ((i)); then
    local concat=""; for ((k = 0; k < i; k++)); do concat+="[p$k]"; done
    ffmpeg -nostdin -v error -y "${inputs[@]}" -filter_complex "${filters}${concat}concat=n=$i:v=1:a=0[v]" -map "[v]" \
      -c:v libwebp_anim -lossless 0 -q:v 62 -compression_level 6 -preset picture -loop 0 -an "$webp"
  fi
  printf 'video:   %s (%s)\npreview: %s (%s)\n' "$mp4" "$(du -h "$mp4" | cut -f1)" "$webp" "$(du -h "$webp" | cut -f1)"
}

# --------------------------------------------------------------------------------------- main
if ((assemble_only)); then
  assemble
  exit 0
fi

need cargo curl hyprctl grim foot bat jq
[[ -n ${HYPRLAND_INSTANCE_SIGNATURE:-} ]] || { echo "demo-video.sh: run it inside Hyprland" >&2; exit 1; }
for s in "${wanted[@]}"; do
  recordable "$s" && [[ " ${scenes[*]%%|*} " == *" $s "* ]] || { echo "demo-video.sh: no scene '$s'" >&2; exit 2; }
done

((reuse)) || cargo build --release --bin tuner --example demo_profile

# --- video: a segment of each NASA source (16:9, 1280×720, silent), or a test pattern
mkdir -p "$media"
nasa=https://images-assets.nasa.gov/video
earth_url="$nasa/Earth%20Views%20from%20the%20International%20Space%20Station/Earth%20Views%20from%20the%20International%20Space%20Station~medium.mp4"
earth_sha256=caf71b75b1cd19816c55179384ea41e606d6ecd1088432b1e8e85e566917ce98   # 55 MB, 1280×720
launch_url="$nasa/Artemis%20I%20Launch%202022%20CU%20tracking%20from%20Press%20Site_compressed/Artemis%20I%20Launch%202022%20CU%20tracking%20from%20Press%20Site_compressed~medium.mp4"
launch_sha256=a02fd793f0b33ce3ec213d9c77eba9426329412f7cd33428422494258af6bb72  # 100 MB, 1280×720
# clip <out> <source name> <url> <sha256> <start s> <length s> <test pattern>
clip() {
  local out=$1 src=$media/$2 url=$3 sha=$4 start=$5 len=$6 pattern=$7
  [[ -s $out ]] && return
  if [[ ! -s $src ]]; then
    curl -fsSL --max-time 600 -o "$src.part" "$url" && mv "$src.part" "$src" || rm -f "$src.part"
  fi
  if [[ -s $src ]] && echo "$sha  $src" | sha256sum -c --quiet -; then
    ffmpeg -nostdin -v error -y -ss "$start" -t "$len" -i "$src" -f lavfi -i anullsrc=r=48000:cl=stereo \
      -map 0:v:0 -map 1:a -vf "scale=1280:720,setsar=1" -shortest \
      -c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p -c:a aac -b:a 32k "$out"
  else
    echo "demo-video.sh: no $2 ($url); using a test pattern" >&2
    ffmpeg -nostdin -v error -y -f lavfi -i "$pattern=size=1280x720:rate=25" -f lavfi -i sine=frequency=440 \
      -t 8 -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -shortest "$out"
  fi
}
# bump a name when its cut changes
earth=$media/demo-nasa-earth-v1.mp4    # 0:49.3–0:57.3, swirling clouds over the limb, between cross-fades
launch=$media/demo-nasa-launch-v1.mp4  # 0:53–1:02.5, SLS clearing the pad's lightning towers
clip "$earth" earth-views-iss-medium.mp4 "$earth_url" "$earth_sha256" 49.3 8 testsrc2
clip "$launch" artemis-1-launch-medium.mp4 "$launch_url" "$launch_sha256" 53 9.5 smptehdbars

# --- artwork for the films and series: public-domain files from Wikimedia Commons listed in
# docs/demo-media.tsv (each sha256-checked), cut to the sizes the demo profile wants: posters
# 400×600, backdrops 1280×720 (a title without a still gets its poster, blurred). A file that
# fails its check is left out, and that title keeps a generated poster.
art=$work/art
prepare_art() {
  local ua="TunerDemoBuild/1.0 (https://github.com/rthoms334/omatuner)" pass slug role title licence sha page url src
  mkdir -p "$media/commons" "$art/poster" "$art/backdrop"
  for pass in poster still; do
    while IFS=$'\t' read -r slug role title licence sha page url; do
      [[ $role == "$pass" ]] || continue
      src=$media/commons/${sha:0:16}.$(tr '[:upper:]' '[:lower:]' <<<"${url##*.}")
      if [[ ! -s $src ]]; then
        curl -fsSL -A "$ua" --max-time 120 -o "$src.part" "$url" && mv "$src.part" "$src" || rm -f "$src.part"
      fi
      if [[ ! -s $src ]] || ! echo "$sha  $src" | sha256sum -c --quiet - 2>/dev/null; then
        echo "demo-video.sh: $slug $role ($page) missing or changed; left out" >&2
        continue
      fi
      case $role in
        poster)
          # a portrait poster fills the card; a landscape one (a half-sheet) sits on a blurred copy
          local fill="scale=400:600:force_original_aspect_ratio=increase:flags=lanczos,crop=400:600"
          if [[ $(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$src" | awk -F, '{print ($1 > $2 * 0.85)}') == 1 ]]; then
            fill="split[a][b];[a]scale=400:600:force_original_aspect_ratio=increase,crop=400:600,gblur=sigma=24,eq=brightness=-0.2[bg];[b]scale=400:-2:flags=lanczos[fg];[bg][fg]overlay=0:(H-h)/2"
          fi
          ffmpeg -nostdin -v error -y -i "$src" -filter_complex "$fill" -frames:v 1 -q:v 3 "$art/poster/$slug.jpg"
          ffmpeg -nostdin -v error -y -i "$src" -vf "scale=1280:720:force_original_aspect_ratio=increase,crop=1280:720,gblur=sigma=28,eq=brightness=-0.12" -frames:v 1 -q:v 4 "$art/backdrop/$slug.jpg" ;;
        still)
          ffmpeg -nostdin -v error -y -i "$src" -vf "scale=1280:720:force_original_aspect_ratio=increase:flags=lanczos,crop=1280:720" -frames:v 1 -q:v 3 "$art/backdrop/$slug.jpg" ;;
      esac
    done < <(tail -n +2 "$root/docs/demo-media.tsv")
  done
}
prepare_art

# --- the demo profile (pristine; every scene runs on a copy). Built now: the guide follows the
# clock, and the agent scene's match is the one on air.
if ((!reuse)) || [[ ! -f $work/profile/demo/refs.env ]]; then
  rm -rf "$work/profile"
  target/release/examples/demo_profile "$work/profile" --media "$earth" --media2 "$launch" --art "$art" --silent
fi
# shellcheck disable=SC1091
source "$work/profile/demo/refs.env"
foot_ini

todo=()
for line in "${scenes[@]}"; do
  name=${line%%|*}
  recordable "$name" || continue
  if ((${#wanted[@]})) && [[ " ${wanted[*]} " != *" $name "* ]]; then continue; fi
  todo+=("$name")
done
# a scene that continues another's profile needs that one first
for name in "${todo[@]}"; do
  from=$(from_for "$name")
  if [[ -n $from && " ${todo[*]} " != *" $from "* && ! -d $runs/$from/profile ]]; then todo=("$from" "${todo[@]}"); fi
done

if ((check)); then
  echo "check: ${todo[*]} (here, without the keyboard focus)"
else
  echo
  echo "About to record ${#todo[@]} scene(s): ${todo[*]}"
  echo "This takes over the focused monitor for about $((${#todo[@]} * 14)) s (workspace \"tuner-demo\")."
  echo "Close shell panels, turn notifications off, keep your hands off the keyboard and mouse;"
  echo "Ctrl+C stops it."
  if ((!assume_yes)); then
    read -r -p "Record now? [y/N] " ok
    [[ $ok == [yY]* ]] || exit 1
  fi
  for i in 5 4 3 2 1; do printf '\rRecording in %s… ' "$i"; sleep 1; done
  echo
  orig_ws=$(hyprctl activeworkspace -j | jq -r 'if (.name // "") != "" and (.name | test("^[0-9]+$") | not) then "name:" + .name else (.id | tostring) end')
fi
if [[ $(hyprctl monitors -j | jq '[.[] | select(.focused)][0].dpmsStatus') != true ]]; then
  echo "demo-video.sh: the screen is off (nothing renders, nothing can be captured)" >&2; exit 1
fi
trap cleanup EXIT
trap 'exit 130' INT TERM
((check)) || goto_workspace "name:tuner-demo"
mkdir -p "$runs"
failed=()
for name in "${todo[@]}"; do
  if record "$name" && cut_scene "$name"; then :; else failed+=("$name"); fi
done
[[ -n $orig_ws ]] && { goto_workspace "$orig_ws" || true; }
orig_ws=""
((${#failed[@]} == 0)) || echo "demo-video.sh: failed: ${failed[*]} (rerun: scripts/demo-video.sh ${failed[*]})" >&2

assemble
