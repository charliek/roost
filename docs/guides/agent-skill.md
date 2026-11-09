# Agent Skill

Roost ships a skill — `skills/roost/SKILL.md` in the repo — that
teaches a coding agent to drive a *running* Roost through `roostctl`:
open a project tab running a command, send it input, read its screen,
wait on its state, watch events, and notify the user. It's the
outbound half of driving Roost from an agent; see [Relation to agent
integration](#relation-to-agent-integration) below for the other half.

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

**Inside a Roost tab, Claude Code already has it.** Roost hands Claude
Code its `roost` plugin through the tab environment
(`CLAUDE_CODE_PLUGIN_DIRS`), and that plugin carries this skill beside
Roost's hooks, as `roost:roost`. There is nothing to install and nothing
is written to `~/.claude`. The copy is the one built into this Roost, so
it always matches the `roostctl` it drives. See [Agent
integration](agents.md).

**Outside Roost tabs, or for another agent**, two routes install the
same skill. They fetch the repository's current copy, so after
upgrading Roost, update the installed skill too.

The general route ([`skills`](https://skills.sh)) installs into Claude
Code, GitHub Copilot, OpenCode, and other agents:

```bash
npx skills add charliek/roost
```

For Claude Code, a native plugin is also available — it namespaces the
skill as `roost:roost`:

```text
/plugin marketplace add charliek/roost
/plugin install roost@roost
```

The marketplace plugin and the plugin Roost hands its tabs share the
name `roost`, so inside a Roost tab Claude loads only one of them, and
`claude plugin list` showing both `roost@roost` and `roost@inline` is
normal. The two carry the same `SKILL.md`.

Either way, install Roost itself first (see the [repo
README](https://github.com/charliek/roost#install)) — the skill is
inert without a Roost to drive.

## `roostctl skill`

Prints the same `SKILL.md` the routes above install, byte for
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

## Relation to agent integration

The skill and [agent integration](agents.md) are the two directions of
the same relationship:

- **Agent integration is inbound.** An agent running *inside* a Roost
  tab — Claude Code, Codex, and the others that guide covers — reports
  its own state to Roost, so the tab dot, the sidebar rollup, the Agents
  panel and desktop banners reflect what that agent is doing.
- **The skill is outbound.** An agent — the same one, or a different
  one, running anywhere it can reach the socket — drives Roost: opens
  tabs, sends input, reads output, waits on state, gets notified.

A Claude Code instance running inside a Roost tab has both from the same
plugin: its own turns show up on the tab dot, and the skill lets it open
*other* tabs and orchestrate them. Switching Claude Code off under
**Agent Integration…** removes both from new tabs. Anywhere else, the
skill installs on its own and needs no hooks.
