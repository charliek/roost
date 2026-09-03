# `tools/session/live/` — the ssh reconnect ladder, live

Six lanes and five negative controls that drive a **real** `sshd` through
a **real** severance and read every claim back through `host.status`.
This is the only harness that exercises the auto-reconnect ladder against
the OS: `tools/roosttest/test_host_ssh.py` covers the same behaviours
against a fake `ssh`, in CI, on every push — this one is what proves the
fake is telling the truth.

**Not a CI lane.** It needs `sudo` firewall rules against a live `sshd` on
the machine it runs on, and L2 spends the production attempt budget (~8
minutes). Runs by hand in a shed.

```
./live.sh L1     recover:  sever, watch the ladder climb, restore, come back unaided
./live.sh L2     settle:   sever and leave it down, watch the give-up (~8 min)
./live.sh L3     hostkey:  a changed host key is never retried
./live.sh L4a    quit while a retry is ARMED
./live.sh L4b    quit while an establish is IN FLIGHT
./live.sh L5     refused:  stop sshd instead of black-holing the route

./mutate.sh selftest   all five controls; green only if every one trips
```

`PASS` is the last line of a green lane, and `mutate.sh` prints
`PASS (control)` when the assertion it targets correctly **tripped**.

## Setup

In a shed whose landing dir is the mounted repo (`roost-dev`):

```bash
ROOST_REPO=$HOME/roost CARGO_TARGET_DIR=$HOME/rt \
  ROOST_SHED_PACKAGES="roost-iced roost-cli roost-session" \
  ~/roost/tools/shed/build-in-shed.sh   # roost-session is the far side of the bridge
sudo apt install jq                   # once, on a shed provisioned before jq was added
sudo loginctl enable-linger "$(id -un)"   # once; see below
tools/session/live/live.sh L1
```

`roost-session` has to be on the remote's **non-interactive** `PATH` (on
`roost-dev`, `~/.local/bin/roost-session` symlinks into `~/rt/debug`) —
that is what makes `localhost` a realistic remote.

### The far side: where the daemon has to be listening

Preflight **starts the session daemon itself** and refuses to run a lane
until it answers *where the bridge looks*. Those are two different
places, and conflating them is how these lanes came to depend on a
daemon somebody else had started:

* the app's remote command is `roost-session client-bridge`, which
  resolves its socket through `roost-ipc`'s one rule (`paths.rs`,
  `resolve_paths_linux`) — `$XDG_RUNTIME_DIR/<namespace>/roost.sock`
  when that variable is set, non-empty and absolute, else
  `/tmp/<namespace>-<uid>/roost.sock`;
* the `XDG_RUNTIME_DIR` it reads is the **remote non-interactive
  login's** (`pam_systemd` sets `/run/user/<uid>`), not this harness's —
  and the harness exports a scratch `XDG_RUNTIME_DIR` of its own so a
  lane's *UI* socket can never collide with a developer's real one.

So preflight reads the remote's value over ssh (`ssh <target>
'printf %s "$XDG_RUNTIME_DIR"'`), starts `roostctl session start` under
exactly that (unset when the remote's is unset, empty or relative — the
three cases the resolver treats alike), and then proves reachability by
running `roostctl session status` **over ssh**, so the check is the
bridge's own resolution rather than a re-derivation that could agree
with a wrong answer. A daemon that was already listening there is used
as-is and left alone; only one preflight started is stopped again in
teardown, and only while `session_id` still matches.

That is what `enable-linger` is for: `/run/user/<uid>` is created per
login and removed with the last one, so without it the daemon's socket
directory disappears underneath it — possibly moments after preflight's
own ssh created it. So for a `/run/user/…` runtime dir preflight checks
**both** that the directory is there and that `loginctl` reports
`Linger=yes`, and refuses with the exact command to run otherwise. It
names the remedy rather than taking it: lingering is a decision about
the box, not about a test run.

| Parameter | Default | |
|---|---|---|
| `ROOST_RT` | `$HOME/rt/debug` | where `roostctl` and `roost-iced` are (matches `build-in-shed.sh`) |
| `ROOST_LIVE_TARGET` | `ssh://shed@localhost` | the host the lanes drive; the probes dial exactly this |
| `ROOST_LIVE_ART` | `<repo>/target/live-ssh` | per-lane output and the V5 captures |

### The firewall rules, and what preflight refuses

Severance is `iptables`/`ip6tables` DROP rules on port 22, and a firewall
is shared machinery. So **every rule the harness inserts is tagged**:

```
-A OUTPUT -p tcp -m tcp --dport 22 -m comment --comment roost-live-harness -j DROP
-A INPUT  -p tcp -m tcp --sport 22 -m comment --comment roost-live-harness -j DROP
```

Cleanup deletes by that full spec, tag included, so it takes back exactly
what it installed. A port-22 DROP rule an administrator put there is
never a candidate: the `-D` cannot match it. (`mutate.sh M1` installs the
same tagged rules, IPv4-only — that is the whole of its mutation.)

The other side of the same fence: **preflight refuses to run a lane while
an untagged port-22 DROP rule exists.** It prints each one verbatim, as
`iptables -S` spells it, and stops —

```
[L1] port-22 DROP rules this harness did not install (no 'roost-live-harness' tag):
    iptables -A OUTPUT -p tcp -m tcp --dport 22 -j DROP
[L1] FAIL: refusing to start: this box is already partly severed by 1 rule(s) …
```

It refuses rather than deleting because removing somebody else's firewall
rule is the failure the tag exists to prevent, and it refuses rather than
continuing because a box that is *already* partly severed makes every
"the route was live, then I cut it" claim a claim about someone else's
rule. Remove or tag the rule yourself, then re-run.

## What holds these lanes up

* **State comes from the op that owns it.** Every connection claim is a
  `roostctl host status --json` read — `generation` (attempts *started*,
  the monotonic edge a poll cannot miss), `state`, `rollup` (the sidebar
  band's own output), and `retry` (present only while a rung is armed).
  Nothing reads what the app printed. ssh processes and scratch
  directories are still counted directly: those are OS facts, not log
  lines.
* **The band agrees with the row it came from.** At every ladder poll
  that carries a `retry`, `rollup` must equal
  `disconnected — reconnecting in {ceil(delay_ms/1000)}s ({attempt}/{budget})`
  with all three numbers read from that same response — §7 AC1's format
  agreement, asserted here against a real ladder rather than a fake
  `ssh`. The seconds mirror `host_conn.rs`'s `retry_line`: rounded up,
  floored at one second.
* **The app is a direct child.** The harness runs `Xvfb` itself and
  starts `roost-iced` under it, so a SIGTERM lands on the process under
  test and `wait` returns its exit status — which is the whole proof
  that the graceful quit path ran.
* **The harness owns its far side.** Preflight starts the session
  daemon under the runtime dir the *bridge* resolves and proves it
  reachable through that same resolution, so a lane never quietly
  borrows a daemon somebody else left running (see Setup).
* **Every claim is asserted.** A lane that echoes a number nobody
  compares passes while the feature is broken; this harness reported
  three false passes before that rule existed.
* **The assertions are themselves tested.** `mutate.sh` breaks each
  trap on purpose and requires the check to notice, and it drives the
  *same* predicates out of `lib.sh` that the lanes assert on. Each
  control proves its **precondition** first, because a control that trips
  for the wrong reason proves nothing:

  | | the trap | precondition | must trip |
  |---|---|---|---|
  | `M1` | `localhost` is `::1`, so IPv4-only rules drop nothing | `getent ahosts localhost` starts with `::1` | `probe_severed timeout` |
  | `M2` | signalling the display, not the app | an establish is in flight | the graceful-exit proof (`wait` ≠ 0 **and** leftovers) |
  | `M3` | a flatness window shorter than the ladder's own gaps | a rung actually fired | `assert_flat` |
  | `M4b` | no exit path, so what the app owned leaks | an establish is in flight | the leak half of the same proof |
  | `M5` | `ssh.socket` re-activates a stopped `ssh.service` | `ssh.socket` is `active` | `probe_severed refused` |

## What is outside the contract

The lanes assume the machine keeps running while they do. `retry.attempt`
is asserted to climb monotonically within an outage, and a **suspend**
resets the ladder by design — so suspending the box (or the VM) mid-lane
would fail that assertion on behaviour that is correct; run these on a
machine that stays awake.
