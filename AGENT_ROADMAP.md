# Roost — Agent Integration Roadmap

Roost's differentiator is hosting **many AI coding agents at once** and
routing their attention correctly. This document is the live plan for
that surface: the state model underneath it, the decisions already
pinned, and the ledger of implementation plans that build it out.

This supersedes the agent-related rows in [`FEATURES.md`](FEATURES.md),
which predates the current codebase and describes several things that
have since shipped (command palette, notification inbox, drag-to-reorder).
Treat `FEATURES.md` as historical; treat this as current.

See [`docs/development/vision.md`](docs/development/vision.md) for the
overall architecture and decision log — the agent-specific decisions
below are recorded there as DL entries once implemented.

---

## The problem this roadmap solves

As of the analysis in [#258](https://github.com/charliek/roost/issues/258),
the entire agent model is **two fields** on `Tab`:

* `state: TabState { none, running, needs_input, idle }`
* `hook_active: bool`

Three unrelated writers share the single `state` field — OSC 133 shell
marks, Claude Code hooks, and manual `roostctl tab set-state`. The
`hook_active` boolean does double duty as an OSC gate *and* a sidebar
rollup suppressor. Every new agent signal therefore either fights the
existing ones or needs a parallel field plus re-derived precedence in
two UIs.

That is the bottleneck. Almost every agent UX feature worth building —
severity colors, per-agent identity, an agent overview, an agent
switcher — is blocked on there being somewhere coherent to put the
information.

This was the state of the model before plan 002. L0 + L1 below (the
four axes, the derivation, and the `roost-agent` adapter seam) have
since shipped — see the [plan ledger](#plan-ledger) — so this section
is retained as the motivating history, not the current model; the
current one is documented in [`docs/reference/ipc.md`](docs/reference/ipc.md).

---

## The layers

Work is sequenced in four layers. Each depends on the one before it.

### L0 — State model *(both UIs, at parity)*

Replace the single overloaded `state` with four independent axes:

| Axis | Values | Owner |
|---|---|---|
| **Shell state** | `unknown` / `at_prompt` / `foreground_process` | OSC 133 marks |
| **Agent lifecycle** | `inactive` / `working` / `waiting` / `finished` / `failed` | Agent adapters |
| **Attention** | pending flag + title/body + severity | Notifications |
| **Ownership** | `{ source, session_id, last_event_at, detail, metadata }` | Agent adapters |

The displayed state is **derived** from these, never written directly.
`tab.state` stays on the wire as a derived, read-only field so existing
callers keep working.

### L1 — Agent adapter seam *(Rust only, one implementation for both UIs)*

A `roost-agent` crate owning per-agent event→op translation. The Claude
adapter gets what it's missing today: `notification_type` awareness,
`StopFailure` (field is `error` + `error_details`), `background_tasks` /
`session_crons` so a session paused waiting on background work is not
announced as finished, the `session_id` a report needs for the
workspace's own scoping (the adapter stays pure — it carries the id,
it doesn't enforce the match), and an `agent_id` filter as
defense-in-depth for events that fire inside subagents. Adding a second
agent becomes a new module plus a new `roostctl <agent> install` — zero
Swift, zero GTK, zero parity risk.

### L2 — Diagnostics

`roostctl doctor` — shell integration loaded? marks supported by this
shell? socket reachable? hook commands point at a real executable? what
are this tab's four states? Today every agent-integration bug is
diagnosed by reading source, which is the single largest tax on
iterating here.

### L3 — Agent UX *(the payoff)*

The features that motivate the whole roadmap. Cheap once L0–L2 exist —
see [Direction](#direction--candidate-l3-features) below for each one's
requirements on the earlier layers.

---

## Pinned decisions

These are settled. Plans implement them rather than re-litigating them.

**AD-1 — Derived state, not written state.** Effective state is a pure
function of the four axes: agent lifecycle wins when a live session is
present, otherwise shell state, otherwise nothing. Implemented twice
(Swift + Rust) but driven by a shared fixture table.

**AD-2 — Shared golden fixtures for parity.** The derivation and rollup
ranking are pinned by language-neutral fixtures at
`tests/agent-state-fixtures/`, loaded by both the Swift and Rust test
suites. This is the same pattern already proven by `tests/word-fixtures/`
and `tests/url-fixtures/`. Behavioral drift between the two UIs surfaces
as a red test rather than a bug report.

**AD-3 — Ownership is a label, not a suppression switch.** Because
effective state falls through to shell state when no live agent state is
present, a stale owner contributes nothing and degrades *cosmetically*
(the tab is still labeled "claude" when Claude isn't running). This
removes the need for a heartbeat or TTL. Ownership clears on matching
`SessionEnd`, on supersede by a new `session_id`, and on PTY replacement
(tab close, and hard-restart when [#170](https://github.com/charliek/roost/issues/170)
lands).

**AD-4 — Raw-OSC suppression is the one real suppression, so it gets the
failsafe.** Suppressing OSC 9/99/777 while an agent session owns a tab is
the only place stale ownership is actually harmful. That gate — and only
that gate — additionally releases on an OSC 133 `A`/`B`/`D` mark (any
mark meaning "at a prompt"), which also drops the agent lifecycle to
`inactive` so derivation falls through to shell state, not just the
raw-OSC gate. Everything else follows AD-3.

**AD-5 — Optimize for working shells.** Roost targets users whose shell
emits OSC 133 marks. `unknown` stays a distinct shell state precisely so
`roostctl doctor` can *report* an incapable shell (e.g. Apple's
`/bin/bash` 3.2) rather than the UI silently showing nothing forever. Do
not design to the lowest common denominator; surface it instead.

**AD-6 — Notification policy: suppress banner, unread badge, and the
inbox row together when focused.** A notification for a tab that is
active in an active window fires no banner, sets no unread badge, and
adds no inbox row — the three are decided as one transaction at
arrival, not filtered independently per surface. They're coupled on
purpose: inbox membership derives from the same pending bit the badge
does, so "keep the row but drop the badge" isn't expressible without a
separate notification-history store, which is the durable-inbox
non-goal below. The sticky agent-state indicator is the one exception —
*not* focus-cleared, so the "I walked away" case is still covered by
the tab's state dot. Severity may override the suppression later
(`failed` should interrupt regardless); the model carries `severity`
now even though v1's policy doesn't yet consult it.

**AD-7 — One extensible report op, not many narrow setters.** Agents
report through a single `tab.agent_report`. Its fixed fields cover the
axes (ownership action, lifecycle, attention, severity); an open
`metadata` string map is the genuinely additive channel for anything a
specific agent needs later, because the request struct's
`deny_unknown_fields` means a new *named* field on the op is not
actually backward-compatible — both server implementations would have
to change, where a new `metadata` key costs neither.

**AD-8 — `source` is an open string.** Not a closed enum. A third-party
agent must not require a wire-schema change.

---

## On the Swift/Rust duplication

`Workspace`, rollup, OSC routing policy, key encoding, mouse routing, and
palette logic are all implemented twice. The strategic answer is a
`roost-core` Rust crate behind a C ABI that Swift links — a pattern
already proven in this repo by `roost-vt`/libghostty-vt.

**That is not this work.** But this work should not make it harder, and
should ideally make it easier. Concretely: the derivation stays a pure
function over a plain struct with no I/O and no platform types (the most
liftable shape available), and AD-2's shared fixtures mean an eventual
consolidation already has its equivalence proof written.

---

## Plan ledger

Each row links a gauntlet plan. Plans live outside the repo per the
gauntlet convention; the PR body carries the durable public record.

| # | Scope | Plan | Status | PR |
|---|---|---|---|---|
| 002 | L0 state model + L1 agent adapter + parity fixtures + e2e lifecycle | `~/.claude/plans/roost/002-agent-state-model.md` | Shipped | [#259](https://github.com/charliek/roost/pull/259) |
| — | L2 `roostctl doctor` + diagnostics | *not yet written* | Next | — |
| — | L3 agent UX (tint / overview / switcher) | *not yet written* | Future | — |

---

## Direction — candidate L3 features

**These are directional, not committed.** They are recorded here so the
earlier layers build the right hooks rather than discovering them late.
Each entry names what it needs from L0–L2; if a requirement is missing
when its layer ships, that feature gets more expensive.

The metadata listed below is **real** — verified against the Claude Code
2.1.220 hook schemas (see the plan's current-state section). It is
available today and simply has nowhere to go until L0 lands.

### D1 — Per-agent tab tint

Optionally color a tab by which agent owns it (e.g. orange for Claude),
so a wall of tabs is scannable without reading titles.

*Needs:* `ownership.source` as an **open string** (AD-8), present on the
`Tab` snapshot and emitted on change. A config surface mapping
`source → color`, opt-in. Nothing else — this is mostly UI.

*Note:* AD-3 makes a stale owner cosmetic, which is exactly the failure
mode here — a tab tinted for an agent that already exited. Acceptable.

### D2 — Agent overview

A surface listing every live agent with status and metadata, so "what is
everything doing right now" is one glance instead of a tab crawl.

*Needs:* the full agent record on the `Tab` snapshot (not behind a
separate query), plus `last_event_at` to render age. Both land in L0
specifically for this. Richer fields it can show, all available from
hooks: `model` and `session_title` (`SessionStart`), `permission_mode`,
`cwd`, in-flight `background_tasks`, and compaction state
(`PreCompact` / `PostCompact` carry `trigger` and `compact_summary`).

*Sequencing note:* the free-form `detail` field, plus the open
`Ownership.metadata` map (both shipped with L0/L1 — see
[`ipc.md`](docs/reference/ipc.md)), mean this data can already be
surfaced without a new op (AD-7). What's missing for D2 is purely the
UI: a surface to render it.

### D3 — Agent switcher palette frame

Jump between agents from the palette, showing directory, name, status,
and age.

*Needs:* nothing beyond D2's data. The existing `view_notifications`
frame is a structurally identical template (one row per interesting tab,
`<project> · <tab>` title, activate → jump), so this is largely
assembly.

*Needs from L0 specifically:* the shared `rank()` ordering, so the
switcher, the overview, and the sidebar stripe agree on what "most
urgent" means. That is why rank is a function pinned by fixtures rather
than an if-chain.

### D4 — Severity-aware notifications

`failed` interrupts even when you are looking at the tab; `finished`
never does.

*Needs:* the `severity` field on the attention axis — which L0 lands
**now** even though the v1 policy (AD-6) does not yet use it. Retrofitting
severity after the fact would mean re-touching both UIs' notification
paths.

### D5 — Task tabs / launch profiles

Saved profiles that open an agent in a project ("Claude in `roost`"),
optionally auto-launching on restore. The `tab.command` column is
already reserved in the schema.

*Needs:* ownership to survive a PTY respawn decision (AD-3 defines
ownership as clearing on PTY replacement, which is the correct default
for a *relaunch* — a task tab reopening should start unowned). Also
benefits from L2's `doctor` to explain a profile that fails to wire up.

### D6 — Per-project attention counts and mute

A count on the sidebar row for how many tabs need attention, and a
per-project mute so eight agents finishing at midnight do not all shout.

*Needs:* `rank()` plus the attention axis being separate from lifecycle
— both L0. Mute is then one flag consulted at the notification boundary.

### D7 — Background-work awareness

Claude's `Stop` reports in-flight `background_tasks` and scheduled
`session_crons`, letting a hook distinguish "done" from "paused, will
wake later". Surfacing that distinction — a tab that is finished vs one
that will resume on its own — is a genuinely new signal no other
terminal shows.

*Needs:* the lifecycle mapping to consult those arrays — **shipped**
with the L1 Claude adapter (`crates/roost-agent/src/claude.rs`'s
`stop()`: non-empty `background_tasks` keeps the lifecycle `working`
instead of `finished`; counts land in `metadata`). What's left for this
candidate is purely a distinct visual treatment in the UI.

### Cross-cutting requirement

Every one of the above reads state and renders it. None of them should
require a new op. That is the entire justification for AD-7's single
extensible `tab.agent_report` plus a rich `Tab` snapshot: **L3 should be
UI work, not protocol work.** If an L3 feature turns out to need a new
op, that is a signal L0 under-delivered.

---

## Out of scope

Restating so they don't drift in:

* **Syncing Claude's `/color` setting** — Claude exposes no hook carrying
  it.
* **Requiring user-specific shell configuration changes** beyond the
  documented shell integration.
* **Integrating every agent up front.** The model must be source-aware
  and extensible; only Claude ships first.
* **A general plugin system.** Providers (`provider =`) already cover
  user scripting; agent adapters are in-tree until a second consumer
  proves otherwise.
