---
name: mac-test
description: Verify roost changes on a Mac, either the one you are on or a remote Mac over ssh. Covers the lane map (which make targets force-quit a running Roost app), checks that protect the human's live session, launching a dev Roost-Iced or Roost.app for a person to try, taking screenshots, the Mac real-input harness (real CGEvents and Accessibility through the never-rebuilt runner app) with `make mac-real-input-check`, the remote recipe (shell, sync by git bundle, lock, locked-screen stop), CI facts and gotchas. Use when asked to run, verify or screenshot Roost on macOS, run e2e-mac, e2e-iced-bundle or e2e-iced-real-input-mac, or test on a Mac you reach over ssh. For Linux see `linux-test` (a shed VM, from a Mac) and `popos-test` (a native Linux box).
---

# Mac testing (local, or a remote Mac over ssh)

Two Mac products exist: the Swift app `Roost.app` (`mac/`, profile `Roost`) and
the iced app `Roost-Iced.app` (`crates/roost-iced/`, profile `Roost-iced`,
bundled by `make bundle-iced`). Most lanes below drive a real UI through the
JSON IPC op set; the real-input harness adds real key and pointer events.
Layers and when to use each: `tools/README.md`.

## Inputs for a remote Mac

These come from the prompt or the agent's memory, never from this file:

- `MAC_HOST`: an ssh destination.
- `MAC_REPO`: the checkout path on that Mac.
- `TEST_SHA`: the commit to verify.

The ssh user must be **the logged-in GUI user** of that Mac. Some other console
user merely being logged in is not enough: grants, LaunchServices and the lock
state are all per GUI session.

## Prerequisites

- `mise install` (rust, zig), then `third_party/ghostty/build.sh` once (cached).
- `uv` (every pytest lane is `uv run --group test pytest ...`). `cargo-nextest`
  is optional (`make which-runner`).
- A logged-in, unlocked GUI session for anything that launches a window.
- Verify the toolchain on the target first: `command -v cargo uv zig`.
  `~/.cargo/bin` is often missing from PATH under **any** login shell, bash
  included, because rustup's env is sourced only interactively. If `cargo` is
  empty, prepend it: `export PATH="$HOME/.cargo/bin:$PATH"`. Shell state does not
  persist between ssh calls, so over ssh prepend it on every call (see the remote
  section).
- `cargo build -p roost-cli` before any e2e or hand-driven probe: the harness
  reuses an existing `target/debug/roostctl` and does not rebuild a stale one.
  Only `make e2e-iced`, `e2e-iced-ci`, `e2e-session` and the `e2e-host-*` /
  `e2e-local-backend` / `e2e-old-session` lanes build it. `e2e-iced-exit`,
  `e2e-iced-menu-quit`, `e2e-iced-clipboard`, `e2e-iced-release-ci`,
  `e2e-iced-bundle`, `e2e-iced-sparkle`, `e2e-mac*`, `e2e-iced-real-input-mac`
  and a bare `pytest` do not: build `roost-cli` first for those.

## Don't break the human's session

Before any destructive lane, or before launching a dev app, **check and report**:

```bash
echo "ROOST_TAB_ID=${ROOST_TAB_ID:-<unset>}"   # set = this agent runs inside a Roost tab
pgrep -fl 'MacOS/Roost'                         # which Roost apps are running now
```

If `ROOST_TAB_ID` is set, the agent's own terminal is hosted by the Roost app
that a destructive lane would force-quit: do not run those lanes, or ask first.

Things to know:

- The Swift app is single-instance and ignores `ROOST_BUNDLE_PROFILE`, so a dev
  `Roost.app` cannot run beside the installed one.
- A dev `Roost-Iced.app` shares the installed app's profile and state, but debug
  builds use the `RoostSessionDev` session dirs, so their `roost-session` never
  collides with a real one.
- `make bundle-iced` rewrites `mac/build/Roost-Iced.app` under any dev app
  running from it. Order: build first, then quit, then relaunch.
- A stale socket or flock is the one cascade failure mode. `roostctl doctor`
  names it.
- Do not let another agent or shell run two UI lanes at once; the lanes share a
  namespace (see the host lanes below).

## Lane map

DESTRUCTIVE means the lane force-quits, or silently reuses, a running Roost app
of that profile. `--roost-fresh` lanes own a hermetic UI and clear a stale one
first.

| Target | What it runs | Destructive | CI job |
|---|---|---|---|
| `make test-rust`, `make check-iced`, `make test-harness` | unit tests, iced lint + boundaries, harness unit tests | no | `rust-build`, `iced-build-e2e`, `harness-unit` |
| `make test-mac` | `swift test` | no | `swift-mac` |
| `make e2e-iced` | curated `ICED_E2E_TESTS` against an Iced UI | reuses a running Iced UI | `iced-build-e2e` (macOS cells) |
| `make e2e-iced-ci` | the same list, `--roost-fresh` (CI parity) | yes (Iced) | `iced-build-e2e` |
| `make e2e-iced-exit`, `e2e-iced-menu-quit` | exit-on-empty and menu-Quit, own invocation each | yes (Iced) | `iced-build-e2e` |
| `make e2e-iced-sparkle` | Sparkle update flow against the bundle, `--roost-fresh`; rewrites the bundle with test update settings | yes (Iced) | `iced-build-e2e` |
| `make e2e-iced-release-ci` | curated subset against a release binary (`ROOST_ICED_BIN`), `--roost-fresh` | yes (Iced) | `iced-release` (Linux only) |
| `make e2e-iced-bundle` | `make bundle-iced`, then smoke + menu bar against the `.app` (`ROOST_ICED_APP`) | yes (Iced) | `iced-build-e2e` |
| `make e2e-mac` / `e2e-mac-ci` | whole `tools/roosttest` against Roost.app (`-ci` adds `--roost-fresh`) | yes (Roost.app) | `e2e-mac` |
| `make e2e-session` | headless `roost-session` daemons, no UI | no | `session-e2e` |
| `make e2e-host-client-ci`, `e2e-host-ssh-ci`, `e2e-host-bootstrap-ci`, `e2e-host-missing-daemon-ci`, `e2e-host-local-spawn-ci` | host-session lanes | yes (Iced) | Linux cells only |
| `make e2e-host-localhost-ci`, `e2e-local-backend-ci` | private `XDG_RUNTIME_DIR` | no | Linux cells only |
| `make e2e-iced-real-input-mac` | real CGEvent/AX input; runs `make bundle-iced` | it owns the `Roost-linux` namespace; rewrites the bundle | `iced-build-e2e` (macOS/wgpu cell, direct mode) |
| `make smoke-mac`, `smoke-iced` | screenshot smoke against a running UI | no | none |
| `make visual-parity` | closes live UIs, captures a hermetic fixture | yes (both) | none |

Rules for the host lanes (`e2e-host-*`, `e2e-local-backend`, `e2e-old-session`):
**one at a time**, never concurrently, and never beside each other: two runs
bind their probes to each other's terminals and look like a product bug. A
non-fresh `e2e-host-client` no-ops its launch if the socket already answers, so a
running installed app is silently tested instead of your build.

The lane lists are enumerated, not globbed. A new roosttest module must be added
to the Makefile list and the CI step lists, and
`tools/roosttest_unit/test_e2e_lists.py` (in `make test-harness`) fails when they
drift. `ICED_CLIPBOARD_TESTS` is appended to the iced lanes only when
`WAYLAND_DISPLAY` is unset (always on a Mac). Hand-run pytest should pass
`$(DAEMON_E2E_DESELECT)`, i.e. `-m 'not session_daemon and not host_client'`,
to match the lanes.

Timeouts: CI sets `ROOST_TEST_TIMEOUT_SCALE=3` and `ROOST_TEST_MODE=1`. Use the
same on a loaded machine. `ROOST_TEST_MODE=1` unlocks the test ops
(`tab.feed_pty_bytes`, `tab.capture_pty_input`).

## Launch a dev build for a human to try

A cold `cargo build` plus `make bundle-iced` takes over 10 minutes, longer than
a foreground tool timeout. Run the build in the background with a log and poll it:

```bash
nohup sh -c 'cargo build -p roost-cli && make bundle-iced' > /tmp/roost-build.log 2>&1 &
# poll until `tail /tmp/roost-build.log` shows the bundle finished and the job is gone
```

Every hand-driven `roostctl` command here goes through `rc`, because an
inherited `ROOST_SOCKET` outranks `--target` and would address the Roost app
hosting your own shell, not the one you launched:

```bash
rc() { env -u ROOST_SOCKET -u ROOST_TAB_ID target/debug/roostctl "$@"; }
```

Iced app, with its dev session. `open` returns at once, so wait for the socket:

```bash
open -n mac/build/Roost-Iced.app
for i in $(seq 1 30); do
  rc --target iced identify --json >/tmp/identify.json 2>/dev/null && break
  sleep 2
done
cat /tmp/identify.json   # bounded: 60 s. Check local_session_socket contains RoostSessionDev
```

Plain `identify` (no `--json`) omits `local_session_socket`. Note the `pid` field.

Quit the dev app by that pid, never by name or bundle id (the installed app
shares both). An answering socket does not prove the pid is yours, so first
check its executable is inside the bundle you launched:

```bash
ps -p <pid> -o command=   # must start with <repo>/mac/build/Roost-Iced.app/Contents/MacOS/
kill <pid>                # SIGTERM, only if it did
```

The dev `roost-session` it started **outlives the app**; stop it with
`rc --target iced session stop`. A debug `roostctl` stops only the
`RoostSessionDev` one, unless `ROOST_TEST_SESSION_DIR_NAMES` is set (it
overrides the debug separation, so leave it unset here). The installed app's
session is untouched.

Swift app: `make bundle`, quit the running `Roost.app`, `open mac/build/Roost.app`,
then `rc --target mac identify`.

## See the app

- `rc --target iced screenshot --out /tmp/shot.png`: rendered in-process,
  no OS permission, works unfocused or occluded. It captures the main window
  only. Under the GL fallback renderer it can return geometry without text
  (#496): check the `Selected: AdapterInfo` line in
  `~/Library/Logs/Roost-iced/roost.log`.
- `tools/screenshot/` (`smoke.sh <mac|iced>`, `pngtool.py` for pixel
  assertions): see its README.
- `screencapture` for native menus and panels, which `roostctl` cannot capture.
  A locked screen blocks it.

## The real-input harness

Real CGEvents and Accessibility (`tools/input/mac/`) drive a Roost-Iced that the
test launches itself, to cover what IPC cannot: Option as Meta, Shift+Enter, the
native popup, full screen, Secure Keyboard Entry, selection auto-scroll, SGR
mouse. Design and limits: `tools/input/mac/test-runner/README.md` and
`docs/development/test-automation.md`.

One-time setup, needing a human at the Mac once:

1. `tools/input/mac/test-runner/build.sh` installs
   `~/Applications/Roost Test Runner.app` and `~/roost-harness/tcc-probe`.
2. Grant the runner Accessibility, Input Monitoring and Screen Recording in
   System Settings (the runner README has the exact commands).
3. **Never rebuild the runner.** A rebuild changes its signature and voids every
   grant; `build.sh` refuses to run over an installed copy.

Before every run:

```bash
make mac-real-input-check      # exit 0 = READY; non-zero prints the blocker
make e2e-iced-real-input-mac   # ROOST_REQUIRE_REAL_INPUT=1: a skip becomes a failure
```

The check reports the four grants, the console lock, Secure Input, the input
source and the display geometry. Its "claimants" line is information only; only
environment blockers fail it. Conditions the run needs:

- unlocked, awake and hands-off (`caffeinate -dimsu`; on a laptop, lid open and
  on power);
- a US input source, Stage Manager off, no switching Spaces;
- any display geometry is fine (notched, unnotched, 1024x768): the full-screen
  test reads each display's insets, and the check prints them for information;
- do not touch the keyboard or pointer during the run;
- other apps may stay open, but a key-taking panel in front of Roost refuses the
  key (that refusal is a failure, not a flake);
- the checkout is off removable volumes and out of `~/Documents`, `~/Desktop` and
  `~/Downloads`, or every rebuild raises a file-access prompt (#599).

**Keep the pointer off the Roost window during IPC-driven drag tests (#606).** A
real-pointer hover hijacks a synthetic drag. Same rule for other apps: keep them
from taking focus.

**A test that holds a pointer gesture starts with no status toast up (#608).**
Check `notice_dump`'s `bottom_line` source is not `"status"`, or the toast's
expiry rewraps the terminal and cancels the gesture.

On a timeout, report state, not just "timed out": the viewport, selection and
what the helper last saw. `app.window_metrics` carries `window_focused` and
`native_focus_losses`, which tell OS focus churn from a real failure; retry only when focus losses rose between press and timeout, never as a
blanket retry, and `_hold_state` in `test_selection_autoscroll.py` is the
pattern to copy.

Ad-hoc real input (one click or key into a running dev app) goes through
`roost-input-mac` run via the runner, targeting by pid only, never by bundle id
(the branch build shares the installed app's bundle id). `roost-input-mac`
subcommands include `preflight`, `window`, `key`, `mouse`, `claimants`,
`capture` and `release-held`; build it with
`cargo build --manifest-path tools/input/mac/roost-input-mac/Cargo.toml --locked`.

## A remote Mac over ssh

**Runner mode only.** In direct mode the responsible process is sshd, which has
no grants. `open` from an ssh shell reaches the logged-in user's LaunchServices,
which is why the runner works remotely. Leave `ROOST_REAL_INPUT_MODE` unset.

**Shell.** Run everything through the user's login shell and prepend
`~/.cargo/bin` on every call (it is missing under bash too, and state does not
persist). Define one local helper and use it for every remote command. The
string passes through two parsers on the Mac (the ssh login shell, then the
inner `"$SHELL" -lc`), so the helper quotes with `printf '%q'` once per layer.
`$MAC_REPO` may hold spaces, `'`, `$` or backticks, and the command passed to
`rr` may contain single quotes; it is run as written, so `$HOME` and `$PATH`
inside it expand on the Mac:

```bash
rr() {  # usage: rr '<command run in $MAC_REPO>'
  local inner
  inner='export PATH="$HOME/.cargo/bin:$PATH"; cd '"$(printf '%q' "$MAC_REPO")"' && '"$1"
  ssh "$MAC_HOST" 'exec "$SHELL" -lc '"$(printf '%q' "$inner")"
}
rr 'command -v cargo uv zig && git rev-parse HEAD'    # compare with TEST_SHA
```

The helper works under a local bash or zsh. The Mac's login shell may be zsh or
bash: the helper emits only backslash escapes (and `$'...'` for control
characters), which both accept. Start long work with `rr 'nohup make ... >
/tmp/run.log 2>&1 &'` so `nohup` runs inside the login shell (from the bare ssh
shell `uv` is missing). `MAC_REPO` must be an absolute path on the Mac (a leading
`~` is not expanded).

**Sync.** Never modify the Mac's working branch; check out the commit detached.
First check the tree is clean, and stop and report if it is not:

```bash
rr 'test -z "$(git status --porcelain)"' || echo "DIRTY: stop and report"
```

```bash
# pushed work (TEST_SHA may be abbreviated)
git push origin <branch>
rr "git fetch origin && git checkout --detach $TEST_SHA"

# unpushed work: a bundle, fetched with a FORCED refspec so a re-sync overwrites the ref
git bundle create /tmp/roost.bundle <branch>
scp /tmp/roost.bundle "$MAC_HOST":/tmp/roost.bundle
rr "git fetch /tmp/roost.bundle +<branch>:refs/remotes/transfer/<branch> && git checkout --detach $TEST_SHA"
```

Here `rr` is given double quotes, so `$TEST_SHA` expands locally (export it, or
paste the value); replace `<branch>` with the real branch name.

**A locked Mac, or one at the login window: stop and report.** If
`mac-real-input-check` says locked, no window server session, or off-console,
nothing over ssh fixes it.

**One lock owner.** The real-input lane takes `/tmp/roost-real-input.lock`
itself (`flock`, non-blocking), so **never wrap it**; wrapping it makes the run refuse to start.
Every other UI lane on a Mac that also runs real input goes under the same lock.
Stock macOS has no `flock(1)`, so use Python:

```bash
python3 -c 'import fcntl,subprocess,sys
f=open("/tmp/roost-real-input.lock","w"); fcntl.flock(f, fcntl.LOCK_EX)
sys.exit(subprocess.call(sys.argv[1:]))' make e2e-iced-ci
```

**Logs.** Write to a file on the Mac and fetch it back. Anything longer than the
ssh timeout runs under `nohup ... > /tmp/run.log 2>&1 &` (inside the login
shell), then poll the file. `nohup caffeinate -dimsu` keeps the Mac awake for a
real-input run.

## CI facts

- Macs run in `iced-build-e2e` (matrix `macos-latest`, renderer `wgpu` and
  `tiny-skia`) and `e2e-mac`. Everything gates through `ci-success`.
- Real input runs in direct mode only in the macOS/wgpu cell, JUnit-guarded:
  `tools/roosttest/junit_guard.py` proves every scenario in
  `test_real_input_mac.py` ran and passed.
- `continue-on-error` steps hide failures in the job conclusion and the steps
  API: read the log body and the diagnostics artifact
  (`e2e-iced-<os>-<renderer>-diagnostics`).
- `ci.yml` has no `workflow_dispatch`: an on-runner loop needs a scratch PR.
- The hosted macOS runners are slower and are scaled by
  `ROOST_TEST_TIMEOUT_SCALE=3`; measure timing questions on the runner, not
  locally.

## Gotchas

- `syspolicyd` stalls fresh test binaries during concurrent cargo builds
  (doctor and panic-hook tests fail): wait for a quiet machine.
- `make e2e-mac` agent lanes fail locally on the system bash 3.2
  (`at_prompt` seed timeouts); baseline against `origin/main` before chasing.
- A long gate in a background Bash dies at its timeout: use `nohup` plus a log.
- `uv run --group test` is how pytest is invoked; a bare `pytest` lacks the deps.
- When a lane fails, report the state (log tail, `identify`, `tab.dump`), not
  "it failed".
