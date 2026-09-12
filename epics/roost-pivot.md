# Epic: Roost Pivot — roost's part

> Not a docs page. This directory is deliberately outside `docs/` so the
> site never publishes it. It is a pointer plus the rules that apply in
> this repo — never a copy of the roadmap.

**Why this exists — read first:**
https://claude.ai/code/artifact/add27f67-3d15-4541-bd3f-eda3f34fcc48
(Private — opens with the owner's claude.ai login. A 404 from anywhere
else is expected, not a broken link.)
Sections that matter here: §03 (the layering rule), §04 Q7 (local-only,
SSH is the transport) and Q12 (where a session lives; the three kinds of
write), §06 Track R.

**The artifact is a dated record and is not being amended — this file is
the current truth.** It was written on 6 September and carries its own
amendments only as far as §10 and §11. Two of its answers have since
been overtaken, so read them against the rules below rather than as they
stand:

* **§04 Q12's "write is three things"** is wholly overtaken now.
  "Semantic writes go through the agent's API" stands. "**Attach** is
  one client at a time, switched by takeover" and "**raw `tab.write`**
  follows attach" were reversed by R15 (#453, landed): a tab admits as
  many data connections as clients dial, `tab.write` and `tab.attach`
  take no lease at all. R20 (#468, landed) then removed the lease
  itself — there is no "foreground" and no takeover left to switch:
  every same-UID connection is symmetric, effects fan out to every
  subscriber and the viewing client applies them, and a tab is muted
  while any client views it. See the "Write is two things" rule below,
  and `vision.md` DL-25 and DL-26.
* **§04 Q7's "no network listener"** is narrower than it reads. It
  governs roost's own transport; an agent's own server on `127.0.0.1`,
  exposed by that agent's plugin, is the agent's lane (R10, landed). See
  the local-only rule below.

Everything else in §04 still reads true. Where the artifact and this file
disagree, this file wins; where this file is silent, the artifact is
still the reasoning.

**Tracking:** https://github.com/users/charliek/projects/4 —
`Epic: Roost Pivot`. Your PR body must contain `Closes charliek/roost#<n>`.
Status moves by itself when that merges. Never edit board status by hand.

## This repo's items

The issue is authoritative; this table is a map.

```bash
gh issue list -R charliek/roost --state open --search "in:title [R"
```

| ID | issue | phase | one line |
|---|---|---|---|
| R1 | [#418](https://github.com/charliek/roost/issues/418) | RP/M3 | re-cut the lease: reads free, `tab.write` + attach owned, takeover keeps the prior reader's stream, `identify` carries a client label |
| R2 | [#419](https://github.com/charliek/roost/issues/419) | RP/M1 | publish `roost-ipc` (or a git-tag policy) with a compatibility policy |
| R3 | [#420](https://github.com/charliek/roost/issues/420) | RP/M5 | the `vt` attach payload kind — already designed, ~60 lines |
| R4 | [#421](https://github.com/charliek/roost/issues/421) | RP/M5 | scrollback on `tab.dump` |
| R5 | [#422](https://github.com/charliek/roost/issues/422) | RP/M4 | resume-from-revision for `events.subscribe` |
| R6 | [#423](https://github.com/charliek/roost/issues/423) | RP/M2 | grok adapter: `Stop` re-fires per continuation → false idle; add `StopFailure` |
| R7 | [#424](https://github.com/charliek/roost/issues/424) | RP/M6 | hygiene: lifecycle ops, ~~autostart artifact~~ (removed by R16), dead `pty_replaced` path |
| R8 | [#425](https://github.com/charliek/roost/issues/425) | RP/M2 | gx opt-in indicator via the `metadata` map — no new agent variant |
| R9 | [#426](https://github.com/charliek/roost/issues/426) | RP/M5 | local default flip (HS-5): the UI connects to a local `roost-session` |
| R10 | [#439](https://github.com/charliek/roost/issues/439) | RP/M3 | a bare `opencode` binds no server, so nothing can drive the session roost reports — the plugin serves a loopback proxy and names it |
| R11 | [#442](https://github.com/charliek/roost/issues/442) | RP/M4 | the iced client still re-snapshots on reconnect, so R5's resume buys roost's own UI nothing |
| R12 | [#443](https://github.com/charliek/roost/issues/443) | RP/M6 | ~~autostart's first cut: a dev install repoints a release one, status cannot say whether it is enabled, reboot survival is two steps~~ — superseded by R16 |
| R13 | [#444](https://github.com/charliek/roost/issues/444) | — | CI: `pty_shutdown_test` cannot allocate a PTY on macOS, failing `rust-build` on unrelated diffs |
| R14 | [#447](https://github.com/charliek/roost/issues/447) | RP/M5 | a `vt` fallback is silent and unactionable: nothing in the UI says fidelity dropped, and a remote host has no in-app way to update its daemon |
| R15 | [#453](https://github.com/charliek/roost/issues/453) | RP/M5 | open `tab.write` and `tab.attach` to every same-UID client, tmux/herdr style; the lease stays as the *foreground* (effects, focus, geometry), never as an input gate |
| R16 | [#454](https://github.com/charliek/roost/issues/454) | RP/M6 | remove `roostctl session autostart`: sessions come up on demand from the connecting client; supersedes R12 |
| R17 | [#462](https://github.com/charliek/roost/issues/462) | RP/M5 | R15's in-place foreground retake never runs over SSH — a reconnect rebuilds the bridge socket, so the remote case falls back to the reattach R15 exists to avoid |
| R18 | [#463](https://github.com/charliek/roost/issues/463) | RP/M5 | `tab.resize` zeroes libghostty's cell metrics; R15's whole-tuple geometry compare made that reachable, so in-band size reports read `0x0` |
| R19 | [#461](https://github.com/charliek/roost/issues/461) | — | CI/dev: `runtime_dir_test` fails 9/12 under umask 0002 — its own tempdir fixture is group-writable, and `roost-session start` refuses the same way |
| R20 | [#468](https://github.com/charliek/roost/issues/468) | RP/M5 | retire the lease: session protocol 5, no `session.connect`, no `driver_changed`; every same-UID client is symmetric — closes #458 and #466 |

**Sequencing.** R6 went first, as the XS item that shook down the
issue → PR → `Closes` → board chain; R2 and R1 followed, then R8, then
R7 and R5, then R3 and R4 together, then R10. What is left is
**R9, the one not to rush** — it depends on R1 (landed) and changes the
default every session runs under, so it wants living with the pieces
before it, not speed. The board carries the current state; this
paragraph is only the order and the reasoning behind it.

**R11–R14 are follow-ups from shipped work, and they land before the
mobile surface is announced** (the autostart one was removed by R16).
R12 is one pass over what R7 shipped — a defect plus the two things that
first cut left unfinished — R11 is the client half R5 did not build, and
R13 is a macOS CI flake
that fails unrelated pull requests, because a red job nobody reads is how
a real regression gets waved through.

R14 is R3's own cost, and it is the one to read carefully before
scheduling. R3 traded a loud failure for a quiet degradation: a build
skew used to stop the client dead with a dialog, and now it connects and
works at lower fidelity — links stop working, a full-screen program
leaves a blank shell behind, wrapped lines replay as separate lines. The
only way to know is `roostctl host status --json`. Two halves follow from
that, and **the visible one is worth more than the button**: an
indicator, so the degradation is not silent; and an on-demand
restart-or-update action, because R3 removed the state the existing
"Update roost-session on ‹host›" offer was raised from, leaving a remote
host no in-app route. Its trigger is narrower than the issue title
suggests — the restart dialog still fires on a *protocol* mismatch, which
moves more often than the Ghostty pin, so R14 only bites when the pin
moved and the protocol did not.

**R15 reverses one of R1's two halves, on purpose.** R1 made reads free
and writes owned. Measured against tmux and herdr (2026-09-08), owned
input is stricter than either substrate for no product gain: tmux lets
every attached client type, and herdr's ordinary multi-pane client mode
gates no input either — its foreground client is derived from the newest
activity stamp rather than claimed, and carries bells, title and sizing
without ever deciding who may type. (herdr does keep a single writable
owner with an explicit `--takeover`, but only for a raw direct attach to
one terminal; see DL-26, which corrects an earlier overstatement that
herdr had no owner at all.) Roost follows. Every same-UID client
may type into and attach to any tab; the lease survives as the
foreground. The wire change is additive at protocol 4 — shed pins one
identify vector per generation and must not need a re-pin. Geometry is
the herdr rule, last interactor wins; smallest-wins was rejected
because a phone glancing at a tab would shrink the desktop.

**R16 removes what R7 built and R12 was hardening.** A session comes up
when a client asks for it — the localhost launch ladder, the SSH
bootstrap ladder, or `roostctl session start` — and R9 makes the first
of those the app's default. Nothing needs a session up before a client
exists, a shed included: it gets a `roost-session` when a client joins
it, like any machine. The supervisor artifact was costing a 2,500-line
hardening pass with no consumer; it comes back only with a case that
needs it named in its own acceptance box.

**R17–R19 are R15's own follow-ups, and R17 is the one that matters for
the flagship.** R15 made a takeover stop freezing the desktop and gave
the deposed client its foreground back *in place* — no reconnect, no
reattach, no blink. That holds on `localhost` and falls through on SSH:
reconnecting there rebuilds the tunnel and the bridge socket underneath,
so there is no surviving connection to claim the foreground on and the
button does the full reattach R15 exists to avoid. The outcome is
correct either way, which is why it is a follow-up and not a defect in
R15 — but the product is *"start it on the desktop, pick it up on the
phone, sit back down,"* and the phone reaches a real machine over SSH.
Shipping the multi-device story with the desktop half polished and the
remote half degraded is shipping it backwards. R18 is R15's own cost in
the same sense R14 was R3's: zeroing the cell metrics on `tab.resize`
predates it, and comparing geometry as a whole tuple is what made an
already-attached client's size report go to `0x0`. R19 is hygiene of
R13's kind — a gate that is red for an environmental reason on the box
people develop on, which is how a real failure gets waved through — and
it earns its row because the same check refuses a `roost-session start`,
so it is not only a test.

One already on the board is worth reading beside these rather than
below it: **#460** (no roosttest fixture for a daemon in the UI's own
localhost profile) is named in its own body as work to do *before* R9,
since R9 turns localhost from a minority case into the path every session
takes. **#458** (a subscribe's control and event legs can name
different sessions) landed with R20: the ack now carries `session_id`
and a mismatched pair is refused before the pump starts.

**R20 removes the residue R15 left standing.** R1 made reads free; R15
made input free; what remained was "the foreground" — four things a
lease decided (which stream got `tab.effect`, whose focus muted
notifications, who `session.driver_changed` named, and which
connection the settings ops accepted). Measured against tmux and
herdr, that residue bought nothing: neither substrate gates anything on
an owner. R20 (#468) retires `session.connect`, the `lease` field on
every op, and `session.driver_changed` — a breaking bump to session
protocol `5`, since nothing built on this wire had shipped past
v0.0.19's protocol `2`. Effects now fan out to every subscriber and the
viewing client applies them; focus is a union over connections, not an
election; geometry stays last-interactor (R15/DL-25, unchanged). It
also closes #466 (a lease published by a diff at a distance) by
deleting every lease carrier the bug depended on, and #458 by requiring
`session_id` on the subscribe ack.

## Rules that apply in this repo

- **Nothing shed-specific enters roost.** Every change here must serve
  roost's own clients too. If it only makes sense for shed, it belongs in
  shed. The `metadata` map on `tab.agent_report` is the extension channel
  — that is where a consumer stamps its own data, by design.
- **Roost stays local-only: UDS + SSH.** No network listener, no auth
  layer. Decided; not a roadmap item; not to be reopened inside this epic.
  The rule governs **roost's own transport** — its sockets, and what
  roost binds. An agent's own server, exposed on `127.0.0.1` by that
  agent's plugin so a client can drive the session roost reports (R10's
  opencode listener), is the agent's, not roost's: it stays on loopback,
  it carries the agent's own auth, and roost's sockets are unchanged.
- **Roost carries status, never conversation content.** "A banner is a
  label, not a transcript" stands. Transcripts come from the agent's own
  protocol, in shed's lane crates.
- **Sessions live in `roost-session`, never in the app's own process**
  (R9). Until R9 lands, work that needs a subscribable session must use a
  host session, not the app window.
- **Every same-UID client is symmetric.** Reads, input, attach and
  settings are free — semantic writes still go through the agent's API
  (many writers, the agent serializes), but nothing else asks who is
  driving, because there is no lease and nothing left to ask (R20,
  #468, landed; amends R1's "writes owned" and R15's "the lease is the
  foreground"). Effects reach every subscriber and the viewing client
  applies them; a tab is muted while any client views it; the last
  interactor sizes the PTY.
- **No supervisor artifact.** A session comes up on demand from the
  client that connects (R16). Do not add a unit, a plist, or a doctor
  probe for one without a case that needs a session before any client.
- The five-agent instrumentation (plan 046) is the status source of truth.
  Do not add screen-scraping as a competing authority; one status
  authority per tab.

## Cross-repo edges

- **R2 → shed S1.** The publish/tag policy is the coordination point;
  shed uses a git dependency until R2 lands.
- **R1 → shed-mobile S3m.** Live push on the phone waits on the lease
  re-cut; polling `tab.list` works before it.
- **R6 ↔ gx A1.** A1 re-verifies gx's hook signals against the fixed
  adapter.
- **R8 ↔ gx A3.** gx stamps the opt-in key once its remote lane exists;
  R8 documents the key and passes it through.
- **R10 → shed A4.** R10 publishes `metadata["server_url"]`; shed's
  opencode lane crate is what consumes it. The lane itself is shed's,
  out of scope here.
- **R9 is roost's own HS-5**, parked in `discovery/host-sessions-roadmap.md`
  as "a decision point, not scheduled." The decision is now made. Until R9
  merges, that roadmap row still reads "not scheduled" — stale by
  decision, not a contradiction: roost's rule is that DL entries land in
  the PR that builds the thing, so the `vision.md` entry and the roadmap
  status update ship with R9, not here.
