# tools/session — a matching `roost-session` daemon on a real remote

The dev loop for exercising the host-sessions bootstrap (Add Host → install
→ connect) against a *real* Linux remote from a Mac, without cross-compiling
and without a `roostctl` install verb. See
[`docs/development/host-sessions.md`](../../docs/development/host-sessions.md)'s
"Dev loop" section for the architecture; this file is the how-to.

## Why a shed, not cross-compile or a container

The repo's stance is explicit: no cross-compile (`linux/scripts/build-deb.sh`
builds amd64 on amd64, arm64 on arm64), no `cross`/`cargo-zigbuild`, and no
Docker container standing in for a Linux toolchain either — amd64-under-QEMU
is slow for a cold ghostty (zig) build and it's a second toolchain path
beside the shed's. A **shed** (Apple VZ Linux microVM,
[`tools/shed/`](../shed)) of the target's own architecture is what CI itself
uses (native runners per arch), so it's what the dev loop uses too:
`roost-dev` (aarch64, local) and a shed on `mini3` (x86_64, remote server).

## Prerequisites

- `jq`, `ssh`, `tar`, and `cargo` on this Mac — `dev-session.sh` uses `jq`
  locally (to parse `shed list --json` and `identify` output copied back
  over ssh); nothing here runs `jq` on the shed itself.
- The shed you're building on must already be **running** (`shed list`, or
  `shed -s <server> list` for one on a remote server) — the script refuses
  rather than starting it for you.
- A shed with Rust + zig provisioned (`.shed/scripts/install.sh` — the same
  install hook `tools/shed/shed-test.sh` uses). That hook also installs
  `jq` on fresh sheds — used by `tools/session/live/` (the SSH reconnect
  harness), not by this loop — but `install.sh` runs **once per shed**
  (`.shed/provision.yaml`'s `install` hook), so an existing shed
  provisioned before `jq` was added won't have it until you `apt install
  jq` by hand once.

## Usage

```bash
# Build roost-session in a shed (roost-dev is the local, aarch64 shed —
# both build host and remote target in one).
tools/session/dev-session.sh build roost-dev

# Copy the built binary back to this Mac, proving it identifies the same
# app_version + libghostty_build as this checkout's own local build.
tools/session/dev-session.sh fetch roost-dev

# Prove the fetched artifact's arch matches a real ssh target before
# handing it to the product's bootstrap.
tools/session/dev-session.sh check ~/.cache/roost/dev-session/roost-session-<v>-linux-arm64 \
  ssh://roost-dev@localhost:2222

# Point a local Roost-Iced.app's bootstrap at the artifact and launch it —
# the app's own consent-card install flow does the rest.
tools/session/dev-session.sh launch ~/.cache/roost/dev-session/roost-session-<v>-linux-arm64 \
  --target ssh://roost-dev@localhost:2222

# All four in sequence.
tools/session/dev-session.sh all roost-dev ssh://roost-dev@localhost:2222

# A shed on a remote shed server (e.g. an x86_64 build box on mini3): pass
# -s before the shed name for build/fetch/all. check/launch take a plain
# ssh target and never need -s.
tools/session/dev-session.sh build -s mini3 roost-build
tools/session/dev-session.sh fetch -s mini3 roost-build
```

## The arch rule

**`fetch` proves the version pin; `check` proves the arch.** `fetch`
refuses unless the shed-side `roost-session identify`'s `app_version` and
`libghostty_build` equal this checkout's own local
`target/debug/roost-session identify` (built via `cargo build -p
roost-session` first if missing) — that's the "cannot match" the loop
exists to catch, before anything ever touches a remote host. It does
**not** prove the fetched binary will run on a given target: `app_version`
and `libghostty_build` are target-independent (the ghostty pin + snapshot
format don't vary by arch), so a green `fetch` is never a substitute for
`check`. `check <artifact> <ssh-target>` reads the artifact's ELF
`e_machine` (offset 18, decoded in the header's own `EI_DATA` byte order)
and compares it against `ssh <target> uname -m`, refusing with both
arches named — and it treats non-ELF input as "not a Linux binary"
outright, stricter than the product's own bootstrap guard (which must
keep accepting the bootstrap test suite's shell-script stubs).

## The two-seam asymmetry

Two env vars point the client at a `roost-session` build, and they are
**not** the same seam:

- **`ROOST_SESSION_BIN`** feeds the **localhost** spawn ladder — the client
  launching its own `roost-session` as a subprocess for `localhost`
  connects. It is **unguarded**: point it at the wrong thing (say, a
  macOS Mach-O) and you get a generic "failed to start", not an arch
  message — there's no remote to compare against.
- **`ROOST_SESSION_INSTALL_BIN`** feeds the **remote** bootstrap (`Add
  Host` → install over SSH). It **is** arch-guarded: the client sniffs
  the file's ELF header against the probed remote arch and refuses at
  `resolve_source`, before a byte crosses the wire, naming both arches.

`dev-session.sh launch` always sets `ROOST_SESSION_INSTALL_BIN` — the
remote-bootstrap seam — never `ROOST_SESSION_BIN`.

## Proving the mismatch refusal deterministically

`fetch`'s identity-mismatch refusal (above) is hard to hit honestly — it
needs a shed whose build is genuinely differently pinned from this
checkout. `ROOST_DEV_SESSION_IDENTIFY_ENV`, if set, is prefixed verbatim
onto the shed-side `identify` invocation:

```bash
ROOST_DEV_SESSION_IDENTIFY_ENV="ROOST_TEST_MODE=1 ROOST_SESSION_FAKE_BUILD=bogus" \
  tools/session/dev-session.sh fetch roost-dev
```

`ROOST_SESSION_FAKE_BUILD` is itself `ROOST_TEST_MODE=1`-gated in
`roost-session` (see `docs/development/host-sessions.md`'s Env seams),
so this reliably manufactures a `libghostty_build` mismatch and proves
`fetch` refuses — no artifact or identity file is left behind on a
refusal.

## Building from an external worktree

`build`'s local-mount path assumes this checkout lives under the main
repo root, mirrored unchanged into the shed's VirtioFS mount at
`~/roost/<relative path>` (`in_vm_repo_path` in `dev-session.sh`). A
worktree checked out somewhere else entirely isn't visible there, so
`build` refuses with a clear error rather than guessing. Set
`ROOST_SHED_REPO` to the in-VM path to use instead:

```bash
ROOST_SHED_REPO='$HOME/some/other/mount/path' \
  tools/session/dev-session.sh build roost-dev
```

`ROOST_SHED_REPO` is trusted verbatim — quote it in single quotes so an
unexpanded `$HOME` reaches the shed (the SHED's `$HOME` should expand it,
not this Mac's).

## The disk precondition

`build` refuses outright below 2G free on the shed's root fs (printing
`du -sh ~/rt* ~/ghostty-src` so you can see where it went — retired
per-plan `CARGO_TARGET_DIR`s left over from other work are the usual
cause) and warns below 4G. It never deletes anything itself — that's a
human decision.

## Cache layout

`fetch` writes into `~/.cache/roost/dev-session/`:

```
roost-session-<app_version>-linux-<amd64|arm64>            the binary (0755)
roost-session-<app_version>-linux-<amd64|arm64>.identity.json   its `identify` output
roost-session-<app_version>-linux-<amd64|arm64>.tree.txt        local HEAD + dirty-file count at fetch time
```

The naming matches the release asset's own (`asset_name` /
`asset_names_are_versioned_per_arch_and_github_safe` in
`crates/roost-ipc/src/bootstrap.rs`) on purpose — it's the same shape a
real install would see. The `.tree.txt` sidecar exists because identity
alone can't prove the local tree was clean: a dirty tree pushed to a
remote shed and built there carries the exact same `app_version` /
`libghostty_build` strings as a clean one.

## What the install itself does NOT do

`launch` never `scp`s a daemon into place. It sets
`ROOST_SESSION_INSTALL_BIN` in the launched app's environment and lets the
product's own bootstrap (Add Host → NotFound → the consent card → staged
verify → atomic commit — see `docs/development/host-sessions.md`) run
exactly as it would for any user. A second installer with its own
semantics is exactly the drift this loop exists to avoid.

## See also

- [`docs/development/host-sessions.md`](../../docs/development/host-sessions.md) — the "Dev loop" section (architecture-level).
- [`tools/shed/`](../shed) — the shed driver this loop is built on.
- `tools/session/live/` — the live SSH-severance harness (ported from plan
  040). A separate tool from this one: this directory gets you a
  *connected* host; `live/` then exercises reconnect/give-up/recovery
  against it.
