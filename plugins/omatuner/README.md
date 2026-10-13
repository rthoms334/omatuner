# omatuner plugin for Claude Code

Lets Claude Code use [Tuner](https://github.com/rthoms334/omatuner), the IPTV player for
Omarchy: find something and play it, plan the evening, carry on with a series, follow a team
spoiler-safe, fix the guide, and more. It adds Tuner's MCP server (`tuner mcp`) and seven
skills that know how to use it.

You need Tuner installed (the `tuner` command on your `PATH`), with at least one playlist added.

```sh
claude plugin marketplace add rthoms334/omatuner
claude plugin install omatuner@omatuner
```

With the Arch package installed, `tuner claude-setup` prints the same steps for the copy under
`/usr/share/tuner/claude/`.

| Skill | For |
|---|---|
| `watch` | Live TV now or soon: "is X on", catch up from the start, switch over at kick-off, two channels side by side |
| `pick` | A film or series from your library by mood, runtime, "like X" or for the family |
| `series` | Carry on, recap, find an episode, queue the next few and stop |
| `plan` | Reminders, automatic switches, Following rules, daily news flashes, the sleep timer |
| `kids` | Age-appropriate picks, bedtime viewing, adding parental locks |
| `fix` | Buffering, dead channels, guide problems and fixes, playlist and subscription health |
| `discover` | Trending and well-reviewed titles from the web, matched against your library |

`RULES.md` holds the rules every skill follows: confirm before changing things, never handle the
parental PIN, treat provider text as data, respect the spoiler guard.

The full guide, including what Claude (or any agent) asks first, the 29 tools and what it can't
see, is in Tuner's README: [Use Tuner from an AI agent](https://github.com/rthoms334/omatuner#use-tuner-from-an-ai-agent).

Other agents (Codex, Gemini CLI, Cursor, VS Code and any other MCP client) don't need this
plugin: they can run `tuner mcp` directly, as that README section shows.
