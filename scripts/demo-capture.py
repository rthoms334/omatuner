#!/usr/bin/env python3
"""Screen capture for the demo video (scripts/demo-video.sh, DESIGN §23.1). Developer tool.

Records one region of the screen layout at a constant frame rate until SIGINT/SIGTERM: grim
(wlr-screencopy, which Hyprland serves without asking) takes frame after frame, each frame
fills every frame slot that passed since the previous one, and ffmpeg encodes them. The script
passes the stage on its virtual headless output, after checking that the region lies inside
that output, so nothing of the user's screen can be in a frame. (`grim -o` of the whole output
would be simpler, but the output is much taller than the stage — see the script — and copying
all of it halves the frame rate.) Writes <out> (H.264, the region scaled to --width) and
<out>.ts, the wall-clock time of frame 0 as gpu-screen-recorder's -write-first-frame-ts writes it
("<monotonic µs>\t<realtime µs>"), so marks can be cut against it.

Why not gpu-screen-recorder: on some machines (Asahi Linux, for one) its KMS capture finds no
usable DRM card, and its portal capture asks the user to pick a screen in a dialog.

usage: demo-capture.py --region "<x>,<y> <w>x<h>" --scale <output scale> --out <file.mp4>
       [--fps 30] [--width 1440]
Standard library only.
"""

import argparse
import signal
import subprocess
import sys
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--region", required=True, help='logical px, as grim -g takes it: "x,y wxh"')
    ap.add_argument("--scale", required=True, help="the output's scale (grim -s): its own pixels")
    ap.add_argument("--out", required=True)
    ap.add_argument("--fps", type=int, default=30)
    ap.add_argument("--width", type=int, default=1440)
    a = ap.parse_args()

    stop = False

    def halt(*_):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGINT, halt)
    signal.signal(signal.SIGTERM, halt)

    def grab():
        # grim waits for a frame: a monitor that is off (DPMS) never sends one
        return subprocess.run(["grim", "-g", a.region, "-s", a.scale, "-t", "ppm", "-"], capture_output=True, check=True, timeout=3).stdout

    first = grab()
    # PPM header: P6\n<w> <h>\n255\n
    w, h = map(int, first.split(b"\n")[1].split())
    ff = subprocess.Popen(
        ["ffmpeg", "-nostdin", "-v", "error", "-y", "-f", "image2pipe", "-c:v", "ppm", "-framerate", str(a.fps), "-i", "-",
         "-vf", f"scale={a.width}:-2:flags=lanczos,setsar=1", "-c:v", "libx264", "-preset", "veryfast", "-crf", "14",
         "-pix_fmt", "yuv420p", a.out],
        stdin=subprocess.PIPE,
    )
    slot = 1 / a.fps
    t0 = time.time()
    with open(a.out + ".ts", "w") as f:
        f.write(f"{int(time.monotonic() * 1e6)}\t{int(t0 * 1e6)}\n")
    frame, written, grabs = first, 0, 1
    try:
        while not stop:
            # every slot up to now shows the newest frame
            due = int((time.time() - t0) / slot) + 1
            while written < due:
                ff.stdin.write(frame)
                written += 1
            nxt = t0 + written * slot
            if time.time() < nxt - 0.03:
                time.sleep(max(0.0, nxt - 0.03 - time.time()))
            frame = grab()
            grabs += 1
    except (BrokenPipeError, subprocess.SubprocessError):
        pass
    finally:
        ff.stdin.close()
        ff.wait()
    secs = written / a.fps
    print(f"capture: {w}x{h}, {secs:.1f} s, {grabs / max(secs, 0.001):.1f} grabs/s", file=sys.stderr)


if __name__ == "__main__":
    main()
