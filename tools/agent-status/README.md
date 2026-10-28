# agent-status

Re-runnable capture harness for the OSC 7501 program-status reports
(and the OSC 133 marks) that coding agents send. Its results are
recorded in [`discovery/agent-program-status.md`](../../discovery/agent-program-status.md);
run it again when an agent's 7501 support changes.

`capture.py` starts one agent TUI in a pseudo-terminal, answers the
common terminal queries, answers `OSC 7501 ; ?` exactly as libghostty
does (`ESC ] 7501 ; ?:states=idle,working,done,blocked,error:kinds=permission,question,auth`,
terminated like the query: BEL gets BEL, ST gets ST), drives a scenario
from `scripts/`, and logs everything with timestamps. Python standard
library only. It runs real agents, so it spends real model tokens; it is
not part of any gate.

## Run one capture

```bash
python3 tools/agent-status/capture.py \
  --name codex-permission --cwd "$(mktemp -d)" --out /tmp/agent-status-out \
  --script tools/agent-status/scripts/codex-permission.json \
  -- codex -s read-only -a on-request
```

`--out` is required and must be outside the repository. Each run writes,
under it:

- `<name>.events.jsonl`: one JSON event per line (`osc7501` with decoded
  fields and base64 `msg`/`title` decoded, `probe7501`, `osc133`,
  `title`, `reply`, `step`, `note`, `exit`);
- `<name>.raw.bin`: the raw PTY bytes;
- `<name>.text.log`: the visible text.

`--env KEY=VALUE` adds a variable after the scrub. `--no-7501-reply`
leaves the probe unanswered, to check an agent stays silent without it.

## Environment

The child's environment drops `ROOST_*`, `CLAUDE*`, `CODEX_COMPANION_*`,
`HERDR_*`, `TMUX*`, `STY`, `ZELLIJ*`, `WEZTERM*`, `KITTY_*` and
`TERM_PROGRAM*`, and sets `TERM=xterm-256color` and `COLORTERM=truecolor`.
Dropping `CLAUDE*` matters: a nested Claude Code inherits the parent's
permission mode, which hides every permission prompt.

## Scenarios

One file per agent and case in `scripts/`. A scenario is a JSON list of
steps (`wait`, `wait7501`, `send`, `sleep`, `quiet`, `optional_wait`; see
`capture.py --help`). The command line belongs to the caller; the ones
the captures used:

| Scenario | Command |
|---|---|
| `claude-permission`, `claude-shell`, `claude-interrupt`, `claude-background`, `claude-startup` | `claude --permission-mode default` |
| `claude-screen-reader` | `claude --permission-mode default`, plus `--env CLAUDE_AX_SCREEN_READER=1` (Claude's OSC 133 marks fire only in Screen Reader Mode) |
| `codex-permission`, `codex-shell`, `codex-interrupt`, `codex-ctrlz`, `codex-startup` | `codex -s read-only -a on-request` (the flags force an approval prompt) |
| `grok-permission` (trust prompt, then a turn), `grok-startup` | `grok`, in a fresh untrusted directory |
| `crush-turn` | `crush` |
| `gx-startup`, `cursor-startup`, `opencode-startup`, `pi-startup` | `gx`, `cursor-agent`, `opencode`, `pi` |

Cases not covered yet (no capture backed them): Claude Ctrl-Z, Grok
shell, interrupt, Ctrl-Z and background, Codex background, and startup
for crush.

Scenarios drive a case with short fixed prompts; they hold no model
output and nothing sensitive. Agents need to be logged in beforehand.

## Tests

`tools/roosttest_unit/test_agent_status_capture.py` covers the decoder,
the probe reply bytes and the environment scrub, and runs the harness
against a fake agent. It runs under `make test-harness`.
