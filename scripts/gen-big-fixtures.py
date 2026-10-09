#!/usr/bin/env python3
"""Generate large deterministic inputs for benchmarks and perf gates (owner: WP-00/architect;
used by WP-02/03/05/06/09/11/13). Never committed.

usage: scripts/gen-big-fixtures.py [OUTDIR]   (default: target/big-fixtures)
         [--channels 100000] [--epg-channels 1235] [--days 7] [--seed 42]

Writes
  big.m3u      100k entries: mixed quoting, CRLF, ;-groups, tvg-shift, catch-up attrs, DRM,
               duplicate names (KeyAllocator), pipe headers, a url-tvg header
  big.xml.gz   epg-channels x days x 48 half-hour programmes (defaults: 1235 x 7 x 48 =
               414 960 programmes ~ the US epg.pw guide), entities, multi-lang titles,
               +0100 / Z / no-zone times
  seed.json    parameters for tests/support/seed.rs-based DB benches (5 000 channels, 24 h)
"""
import argparse, gzip, json, os, random, datetime as dt

ap = argparse.ArgumentParser()
ap.add_argument("out", nargs="?", default="target/big-fixtures")
ap.add_argument("--channels", type=int, default=100_000)
ap.add_argument("--epg-channels", type=int, default=1_235)
ap.add_argument("--days", type=int, default=7)
ap.add_argument("--seed", type=int, default=42)
a = ap.parse_args()
rnd = random.Random(a.seed)
os.makedirs(a.out, exist_ok=True)
groups = [f"Group {i}" for i in range(400)]
countries = ["UK", "US", "DE", "FR", "ES", "IT", "NL", "PL"]

with open(os.path.join(a.out, "big.m3u"), "w", newline="") as f:
    f.write('#EXTM3U url-tvg="http://127.0.0.1/big.xml.gz" catchup="default" catchup-days="3"\r\n')
    for i in range(a.channels):
        q = rnd.choice(['"', "'", '"'])
        g = rnd.choice(groups)
        if i % 50 == 0:
            g = f"{g};{rnd.choice(groups)}"
        name = f"Channel {i % max(1, a.channels - 500)}"  # last 500 names duplicate earlier ones
        extra = ""
        if i % 97 == 0:
            extra += ' tvg-shift="+1"'
        if i % 211 == 0:
            extra += ' catchup="append" catchup-source="?utc={utc}&lutc={lutc}"'
        f.write(f'#EXTINF:-1 tvg-id={q}ch{i % a.epg_channels}.tv{q} tvg-name="{name}" tvg-chno="{i + 1}" '
                f'tvg-logo="http://logo.invalid/{i}.png" group-title="{g}"{extra},{rnd.choice(countries)} | {name} HD\r\n')
        if i % 1000 == 0:
            f.write('#KODIPROP:inputstream.adaptive.license_type=com.widevine.alpha\r\n')
        url = f"http://127.0.0.1/live/u/p/{i}.ts"
        if i % 333 == 0:
            url += "|User-Agent=Mozilla/5.0&Referer=http://ref.invalid/"
        f.write(url + "\r\n")

start = dt.datetime(2026, 10, 9, tzinfo=dt.timezone.utc)
zones = [" +0000", " +0100", "", "Z"]
n_prog = 0
with gzip.open(os.path.join(a.out, "big.xml.gz"), "wt", encoding="utf-8") as f:
    f.write('<?xml version="1.0" encoding="UTF-8"?>\n<tv generator-info-name="tuner-bench">\n')
    for c in range(a.epg_channels):
        f.write(f'<channel id="ch{c}.tv"><display-name>Channel {c}</display-name>'
                f'<display-name lang="de">Kanal {c}</display-name><icon src="http://logo.invalid/{c}.png"/></channel>\n')
    for c in range(a.epg_channels):
        t = start
        z = zones[c % len(zones)]
        for _ in range(a.days * 48):
            e = t + dt.timedelta(minutes=30)
            f.write(f'<programme start="{t:%Y%m%d%H%M%S}{z}" stop="{e:%Y%m%d%H%M%S}{z}" channel="ch{c}.tv">'
                    f'<title lang="en">Show {rnd.randint(0, 99999)} &amp; friends</title><title lang="de">Sendung</title>'
                    f'<desc>Lorem ipsum dolor sit amet.</desc><episode-num system="xmltv_ns">1.{rnd.randint(0, 20)}.</episode-num></programme>\n')
            t = e
            n_prog += 1
    f.write("</tv>\n")

with open(os.path.join(a.out, "seed.json"), "w") as f:
    json.dump({"channels": 5000, "epg_hours": 24, "movies": 50000, "series": 20000}, f)
print(f"written to {a.out}: {a.channels} channels, {n_prog} programmes")
