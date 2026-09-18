# Agent Skill

Roost ships a skill — `skills/roost/SKILL.md` in the repo — that
teaches a coding agent to drive a *running* Roost through `roostctl`:
open a project tab running a command, send it input, read its screen,
wait on its state, watch events, and notify the user. It's the
outbound half of driving Roost from an agent; see [Relation to Agent
Hooks](#relation-to-agent-hooks) below for the other half.

## Scope

The skill's own description states its triggering rule plainly, and
it's deliberately narrow: use it only when the user mentions Roost, or
asks to open, inspect, drive, or wait on a Roost tab or project — never
merely because a task could benefit from a background terminal, a
second shell, or parallel work in general. It also requires a running
Roost: the skill's first instruction is to check with `roostctl
identify`, and to stop and report the error if that fails, rather than
starting Roost or guessing a socket path.

## Install

Two routes install the same skill. `roostctl skill` prints the copy
built into your `roostctl`, which matches that release; an install
route fetches the repository's current copy, so after upgrading Roost,
update the installed skill too.

**The general route** ([`skills`](https://skills.sh)) installs into
Claude Code, GitHub Copilot, OpenCode, and other agents:

```bash
npx skills add charliek/roost
```

**For Claude Code specifically**, a native plugin is also available —
it namespaces the skill as `roost:roost`:

```text
/plugin marketplace add charliek/roost
/plugin install roost@roost
```

Either way, install Roost itself first (see the [repo
README](https://github.com/charliek/roost#install)) — the skill is
inert without a Roost to drive.

## `roostctl skill`

Prints the same `SKILL.md` the two routes above install, byte for
byte — useful for reading it without a package manager, or for an
agent that wants to fetch it directly rather than through a plugin
system:

```bash
roostctl skill              # the skill as markdown, on stdout
roostctl skill --json       # {"topic": "roost", "format": "markdown", "content": "..."}
```

It needs no running Roost — an agent has to be able to read the skill
before it can check whether one is running.

## `roostctl --help` is the syntax authority

The skill deliberately does not bake `roostctl`'s flags and subcommand
shapes into its own text. It tells the agent to read `roostctl --help`
(and the `--help` of whichever verb it's about to use) before that
verb's first call, and never to probe a verb by guessing at required
flags. That's what keeps the skill from drifting out of sync with the
CLI it drives: the skill states the *rules* (which verb for which
task, the target policy, the exit-code table, the recipes), and the
installed `roostctl` states the *syntax*.

## Relation to Agent Hooks

The skill and [Agent Hooks](agents.md) are the two directions of the
same relationship, and neither depends on the other:

- **Agent Hooks are inbound.** An agent running *inside* a Roost tab —
  Claude Code, Codex, and the others Agent Hooks covers — reports its
  own state to Roost, so the tab dot, the sidebar rollup, and desktop
  banners reflect what that agent is doing.
- **The skill is outbound.** An agent — the same one, or a different
  one, running anywhere it can reach the socket — drives Roost: opens
  tabs, sends input, reads output, waits on state, gets notified.

A Claude Code instance running inside a Roost tab typically has both:
its own turns show up on the tab dot through Agent Hooks, while the
skill lets it open *other* tabs and orchestrate them. Neither
Agent Hooks nor the skill requires the other to be installed.
