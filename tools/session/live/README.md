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
ROOST_REPO=$HOME/roost CARGO_TARGET_DIR=$HOME/rt ~/roost/tools/shed/build-in-shed.sh
cargo build -p roost-session          # the far side of the bridge
sudo apt install jq                   # once, on a shed provisioned before jq was added
tools/session/live/live.sh L1
```

`roost-session` has to be on the remote's **non-interactive** `PATH` (on
`roost-dev`, `~/.local/bin/roost-session` symlinks into `~/rt/debug`) —
that is what makes `localhost` a realistic remote.

| Parameter | Default | |
|---|---|---|
| `ROOST_RT` | `$HOME/rt/debug` | where `roostctl` and `roost-iced` are (matches `build-in-shed.sh`) |
| `ROOST_LIVE_TARGET` | `ssh://shed@localhost` | the host the lanes drive; the probes dial exactly this |
| `ROOST_LIVE_ART` | `<repo>/target/live-ssh` | per-lane output and the V5 captures |

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
