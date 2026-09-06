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
| R7 | [#424](https://github.com/charliek/roost/issues/424) | RP/M6 | hygiene: lifecycle ops, autostart artifact, dead `pty_replaced` path |
| R8 | [#425](https://github.com/charliek/roost/issues/425) | RP/M2 | gx opt-in indicator via the `metadata` map — no new agent variant |
| R9 | [#426](https://github.com/charliek/roost/issues/426) | RP/M5 | local default flip (HS-5): the UI connects to a local `roost-session` |

**Start with R6.** It is XS, fixes a live false-idle, and shakes down the
issue → PR → `Closes` → board chain before anything larger depends on it.
R6, R2 and R1 are small enough to ride the release currently in flight;
R9 is the one not to rush.

## Rules that apply in this repo

- **Nothing shed-specific enters roost.** Every change here must serve
  roost's own clients too. If it only makes sense for shed, it belongs in
  shed. The `metadata` map on `tab.agent_report` is the extension channel
  — that is where a consumer stamps its own data, by design.
- **Roost stays local-only: UDS + SSH.** No network listener, no auth
  layer. Decided; not a roadmap item; not to be reopened inside this epic.
- **Roost carries status, never conversation content.** "A banner is a
  label, not a transcript" stands. Transcripts come from the agent's own
  protocol, in shed's lane crates.
- **Sessions live in `roost-session`, never in the app's own process**
  (R9). Until R9 lands, work that needs a subscribable session must use a
  host session, not the app window.
- **Write is three things.** Semantic writes go through the agent's API
  (many writers, the agent serializes). Attach is one client at a time,
  switched by takeover. `tab.write` follows attach. Reads are free to
  everyone. R1 is the two-op swap that makes the lease match this.
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
- **R9 is roost's own HS-5**, parked in `discovery/host-sessions-roadmap.md`
  as "a decision point, not scheduled." The decision is now made. Until R9
  merges, that roadmap row still reads "not scheduled" — stale by
  decision, not a contradiction: roost's rule is that DL entries land in
  the PR that builds the thing, so the `vision.md` entry and the roadmap
  status update ship with R9, not here.
