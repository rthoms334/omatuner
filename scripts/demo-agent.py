#!/usr/bin/env python3
"""The demo video's agent terminal (scripts/demo-video.sh, DESIGN §23.1). Developer tool.

A scripted MCP client: it types two requests the way a person would, then makes the tool
calls an AI agent makes for them against the running demo Tuner, through Tuner's real MCP
server (`tuner mcp --profile-dir <profile>`). Every result it shows comes from those calls;
only the request text and the one-line replies around the results are scripted. Any MCP
client (Claude Code, Codex, an editor's agent…) talks to Tuner the same way.

usage: demo-agent.py --tuner <tuner binary> --profile <profile dir> [--marks <file>]
       [--client <clientInfo name>]

With --marks it waits until the file has an `agent` line (Tuner's `mark:agent` step), so it
starts on cue. Standard library only.
"""

import argparse
import datetime as dt
import json
import os
import subprocess
import sys
import time

# Tokyo Night (the bundled theme the video uses)
FG = "\033[38;2;192;202;245m"
DIM = "\033[38;2;86;95;137m"
BLUE = "\033[38;2;122;162;247m"
GREEN = "\033[38;2;158;206;106m"
MAGENTA = "\033[38;2;187;154;247m"
CYAN = "\033[38;2;125;207;255m"
BOLD = "\033[1m"
RESET = "\033[0m"


def out(text="", end="\n"):
    sys.stdout.write(text + end)
    sys.stdout.flush()


def typed(text, cps=16):
    """Type `text` at about `cps` characters a second."""
    for ch in text:
        out(ch, end="")
        time.sleep(1 / cps)


class Mcp:
    """One stdio MCP session with `tuner mcp` (JSON-RPC, one object per line)."""

    def __init__(self, tuner, profile, client):
        env = dict(os.environ, TUNER_MCP_NO_LAUNCH="1")
        self.proc = subprocess.Popen(
            [tuner, "mcp", "--profile-dir", profile],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            text=True,
        )
        self.next_id = 0
        self.request(
            "initialize",
            {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": client, "version": "1"}},
        )
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise SystemExit(f"tuner mcp stopped during {method}")
            msg = json.loads(line)
            if msg.get("id") == self.next_id:
                if "error" in msg:
                    raise SystemExit(f"{method}: {msg['error']}")
                return msg["result"]

    def call(self, tool, args):
        res = self.request("tools/call", {"name": tool, "arguments": args})
        if res.get("isError"):
            raise SystemExit(f"{tool}: {res['content'][0]['text']}")
        return res.get("structuredContent") or json.loads(res["content"][0]["text"])

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=5)


def when(stamp):
    """'21:30', or 'Fri 20:00' on another day."""
    t = dt.datetime.fromisoformat(stamp)
    today = dt.datetime.now(t.tzinfo).date()
    if t.date() == today:
        return t.strftime("%H:%M")
    if t.date() == today + dt.timedelta(days=1):
        return "tomorrow " + t.strftime("%H:%M")
    return t.strftime("%a %H:%M")


def you(text):
    out()
    out(f" {BLUE}{BOLD}❯{RESET} ", end="")
    time.sleep(0.4)
    typed(FG + text + RESET)
    out()
    time.sleep(0.5)


def shown(v):
    """An argument as the call sends it, shortened for the screen: refs are long and opaque."""
    if isinstance(v, str) and v.startswith(("programme:", "channel:", "movie:", "series:", "episode:")) and len(v) > 22:
        v = v[:18] + "…"
    if isinstance(v, dict):
        return "{" + ", ".join(f"{k}: {shown(x)}" for k, x in v.items() if k != "terms") + "}"
    return json.dumps(v, ensure_ascii=False)


def tool(name, args):
    shown_args = "  ".join(f"{DIM}{k}{RESET} {FG}{shown(v)}{RESET}" for k, v in args.items())
    out(f"   {MAGENTA}●{RESET} {BOLD}{name}{RESET}  {shown_args}")
    time.sleep(0.35)


def result(text):
    out(f"     {DIM}└{RESET} {CYAN}{text}{RESET}")
    time.sleep(0.6)


def say(text):
    out(f"   {GREEN}{text}{RESET}")
    time.sleep(0.4)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tuner", required=True)
    ap.add_argument("--profile", required=True)
    ap.add_argument("--marks")
    ap.add_argument("--client", default="mcp-client", help="clientInfo name: a generic one reads \"your agent\" in Tuner, claude-code \"Claude\", codex \"Codex\"…")
    ap.add_argument("--team", default="Kestrel United")
    a = ap.parse_args()

    out("\033[2J\033[H", end="")
    out(f" {DIM}an AI agent · Tuner's tools over MCP (tuner mcp){RESET}")
    socket = os.path.join(a.profile, "run", "tuner.sock")
    deadline = time.time() + 60
    while not os.path.exists(socket) and time.time() < deadline:
        time.sleep(0.1)
    if a.marks:
        while time.time() < deadline:
            try:
                with open(a.marks) as f:
                    if any(line.startswith("agent ") for line in f):
                        break
            except FileNotFoundError:
                pass
            time.sleep(0.05)
    mcp = Mcp(a.tuner, a.profile, a.client)

    # 1. put the match on
    you(f"put the {a.team} match on")
    args = {"query": a.team, "live_only": True, "limit": 5}
    tool("guide_search", args)
    found = mcp.call("guide_search", args)["programmes"]
    live = next((p for p in found if p["airing"] == "live"), None)
    if not live:
        raise SystemExit(f"no live programme for {a.team} in the guide")
    ch = live["channel"]
    result(f"{live['title']} · {ch['name']} · live until {when(live['end'])}")
    tool("play", {"ref": live["ref"]})
    played = mcp.call("play", {"ref": live["ref"]})
    where = {"popout": "in the pop-out", "window": "in Tuner's window", "device": "on the TV"}.get(played.get("opened_in"), "")
    result(f"{played['outcome']} {where}".strip())
    say(f"It's on: {played['title']}.")

    # 2. follow the team (a write: the agent asks first)
    you("and let me know whenever they play")
    rule = {"name": a.team, "kind": "team", "terms": [a.team]}
    tool("follow", {"action": "preview", "rule": rule})
    preview = mcp.call("follow", {"action": "preview", "rule": rule})
    coming = [m for m in preview["matches"] if m["state"] not in ("skipped", "left_out")]
    for m in coming[:2]:
        on = f" · {m['channel']['name']}" if m.get("channel") else ""
        result(f"{m['title']} · {when(m['start'])}{on}")
    if not coming:
        result("nothing in the guide yet")
    say(f"Follow {a.team}? Tuner reminds you before each match.")
    you("yes")
    tool("follow", {"action": "add", "rule": rule})
    added = mcp.call("follow", {"action": "add", "rule": rule})
    n = added.get("reminders_to_set", 0)
    result(f"following {added['rule']['name']} · {n} reminder{'s' if n != 1 else ''}")
    say("Done. It's in Tuner's Following list.")
    mcp.close()
    # stay on screen until the scene ends (demo-video.sh closes the terminal)
    time.sleep(3600)


if __name__ == "__main__":
    main()
