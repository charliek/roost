# Sessions everywhere: work that doesn't care where you sit

**Status: discovery, 2026-10-23. Partly decided on 2026-10-28. G2
shipped in plan 084 (2026-10-30), see [G2](#g2-window-size-with-several-clients).**
[protocol-7-release.md](protocol-7-release.md) is now the source of
truth for G1, G2 and the new G9, and supersedes the leanings below
where they differ. The [decision summary](#decided-2026-10-28-protocol-7-release)
lists what changed. Everything else here is still undecided. This note
came out of
running a real two-machine setup (a MacBook driving a Pop!_OS box) and
finding where the shipped host-session design stops short of that
workflow. Things checked against code or a live system are cited.
Things inferred and not yet run are marked **(unverified)**.

## The goal

Start or manage a job on any machine, then watch it, type into it, or
pick it up from any other machine. Where you happen to be sitting
shouldn't matter. Concretely:

- Kick off an agent on the Linux box from the Mac, walk over to the
  Linux desktop, and see the same tab live, with the same scrollback,
  and type into it.
- Go back to the Mac and carry on. Neither screen gets in the other's
  way.
- Every machine's own tabs live in a `roost-session` too, so "my
  machine's tabs" and "a remote machine's tabs" work the same way.

**Direction.** Sessions become the normal backend on every machine.
In-process local tabs (`local-backend = in-process`) stay supported for
now. They are a candidate for removal later if few users want them (see
[Retiring in-process local tabs](#retiring-in-process-local-tabs-later)).

**Still non-goals.** These match `docs/index.md` and the project
instructions: no browser or HTML viewer, not multi-window, no split
panes. Also, **a full-screen TUI cannot be laid out differently for two
screens at once**. A program sees exactly one window size per PTY, and
tmux has the same limit. What *can* differ per machine is Roost's own
chrome (sidebar, tab strip, font) and how a client shows a grid that
isn't its own size (G2).

## What already works

Host sessions were designed for multiple devices from fairly early on,
so most of the hard part has shipped.

| Capability | Where it's pinned |
|---|---|
| Any number of clients can attach to one tab at once | R15 / plan 057, `docs/development/host-sessions.md` "Known limitations" |
| Any client can type. No ownership, no "steal" step | DL-25, DL-26 in `docs/development/vision.md` |
| Notifications go to every client; the one viewing the tab acknowledges for everyone | #474, `docs/guides/host-sessions.md` "Several windows on one session" |
| Which tab you're viewing, and per-project tab memory, are per-client and never sent to the session | `docs/guides/host-sessions.md` (troubleshooting, last row) |
| OSC 52 clipboard writes land only on clients viewing that tab | `docs/development/host-sessions.md` "Effects fan out" |
| SSH transport with install and upgrade of `roost-session` on the far end (Linux and Mac) | HS-3, HS-4, #643 |
| This machine's own tabs on a session (the *slot*), with a reversible switch and launch-time migration | plan 063, DL-27 |
| A fresh Roost-Iced install already starts on `session` | DL-27 |

## The field report that prompted this (2026-10-22/23)

The setup: Pop!_OS, `roost` 0.0.23 from apt. A Roost-Iced on a Mac
attaches over Tailscale SSH. The Mac's bootstrap started the daemon
(`~/.local/bin/roost-session start`, pid 276570, up since Oct 9), which
holds 5 projects and 9 tabs.

The question was: *can I sit at the Pop!_OS desktop and see what the
Mac started?* In principle yes, since several clients can attach to one
session. In practice the desktop Roost showed its saved `localhost`
host as:

```text
"state": "disconnected",
"reason": "no session is running at /run/user/1000/roost-session/roost.sock"
```

…while the daemon was serving 4 bridge connections from the Mac at
`/tmp/roost-session-1000/roost.sock`. Pressing Connect tried to start a
second daemon, and the state lock refused it five times:
`another session (pid 276570) is using this state directory`
(`~/.local/state/roost/roost.log`). **Use a session for local tabs**
would fail the same way and back out with "nothing was changed"
(`crates/roost-iced/src/app/local_backend.rs:2158-2170`, `:2765-2781`).
That's safe, but it's a dead end. That is gap G1. Getting past it shows
G2 next.

## Decided 2026-10-28 (protocol-7 release)

The details are in [protocol-7-release.md](protocol-7-release.md).

- **G1:** none of the options below. **The Linux session socket moves
  under the home directory:**
  `<passwd home>/.local/state/roost-session/run/roost.sock`, with a
  `ROOST_SESSION_SOCKET_DIR` override.
  - It uses the passwd home directory, not `$HOME` or `$XDG_STATE_HOME`,
    because those can also differ between logins.
  - The Mac already follows the same "under the home directory" rule.
  - Old paths are probed only to identify and stop an old session
    during an upgrade.
  - Why not `/run/user`: logout deletes it unless linger is on, and the
    session should outlive logins without a service.
  - A shipped user service isn't wanted.
- **G2:** a fuller version of options 1–3. **Explicit size ownership:**
  - deliberate interaction makes a client the owner: an explicit tab
    switch, keys, paste, clicks and wheel (delivered first), a copy
    (after it's captured), a settled scroll, a focused window resize,
    or Take size;
  - just showing a tab, automatic re-attaches, unfocused resizes,
    focus reports and automatic replies don't;
  - other clients' resizes are advisory;
  - when the owner leaves, the most recent advisory grid takes over;
  - background resizes are refused on owned tabs;
  - a `tab.resized` event tells every client.

  This replaces last-interactor (DL-25/26). Rex's owner model was the
  input; typing transferring ownership is the difference.
- **G9 (new, below):** a tab's environment reflects where it was
  opened from.
- **Open questions answered:** "Default size policy" is the ownership
  rules above. "Phones" is [mobile-client.md](mobile-client.md), still
  a native client, not a browser host.
- **G3/G4 (auto-connect, user service, which binary)** are deferred.
  No service is required for the intended use.

## Gaps

### G1. Finding the session depends on `XDG_RUNTIME_DIR` (Linux)

**What happens.** The session socket is
`$XDG_RUNTIME_DIR/roost-session/roost.sock` when that variable is
non-empty and absolute, and `/tmp/roost-session-<uid>/roost.sock`
otherwise (`resolve_paths_linux` and `valid_dir` in
`crates/roost-ipc/src/paths.rs`). The resolver doesn't check that the
directory exists. The code
picks exactly one of the two and never checks the other. Everything
that looks for the session goes through that one resolver:

- the `localhost` host target (`crates/roost-ipc/src/ssh.rs:168-171`);
- `roostctl session start|status|stop` (`crates/roost-cli/src/session.rs`);
- the SSH bridge, `roost-session client-bridge`
  (`crates/roost-session/src/bridge.rs:54-62`).

So two contexts that disagree about `XDG_RUNTIME_DIR` can't see each
other's session.

**Why it bites.** Tailscale SSH's non-interactive exec doesn't set
`XDG_RUNTIME_DIR`. The daemon's environment (`/proc/276570/environ`) has
`SSH_CLIENT`, `HOME` and `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus`,
but no `XDG_RUNTIME_DIR`. A COSMIC (or GNOME, KDE…) desktop login does
set it. Stock OpenSSH with `pam_systemd` normally sets it as well
**(unverified here)**, which is probably why this wasn't hit sooner.
cron, `sudo -u`, and systemd services without `PAMName=` are the same
kind of context.

**What's already known.** #319 found this split and fixed the *safety*
half: the state lock now lives beside `state.json`, so two processes
can't both write to it. The *finding* half was accepted as a limitation.
`docs/reference/architecture.md:129` says the owner's socket is
"undiscoverable in that case by construction", and the
`socket_lock_path` doc comment in `paths.rs` says the same. The
consequence for users is a session that looks dead, plus a misleading
"cannot start" when you try to start it. No open issue tracks this.

**Options.**

| | Change | For | Against |
|---|---|---|---|
| A | When `XDG_RUNTIME_DIR` is unset, derive it from the uid: use `/run/user/<uid>` if it exists, is ours, and is mode 0700; otherwise fall back to `/tmp`. | Small change. Fixes Tailscale, cron and sudo contexts on any systemd machine, for both the session and UI profiles. | Moves a path that `resolve_paths_linux` calls "byte-stable" and pins with golden tests. A daemon already running at `/tmp` across an upgrade would become invisible to new clients unless B or C also lands. |
| B | **Use the state directory as the meeting point.** The daemon writes its bound socket path beside `state.lock`. The state directory follows `XDG_DATA_HOME`/`HOME`, which agree across these contexts. A client whose computed path has no listener reads that record, checks that the lock is held and that `session.identify` answers, and connects. | Finds *any* running daemon whatever the env differences, including ones started by older builds. Lets `session start`'s `StateHeld` refusal say where the session actually is, instead of just refusing. | One more file, with stale-record handling. Doesn't help if `XDG_DATA_HOME` itself differs (rare). |
| C | Check both known paths: the XDG path first, then `/tmp/<ns>-<uid>`. | Smallest change. Covers exactly the case observed. | Hard-codes two locations. A heuristic rather than a rule. |
| D | Have `client-bridge` and the bootstrap's start exec set `XDG_RUNTIME_DIR` (when `/run/user/<uid>` exists) before resolving. | Keeps the change inside the session binary. | Only fixes contexts Roost itself launches. A hand-run `roostctl session start` over Tailscale still splits. |

**Leaning:** A going forward, plus B (or C at minimum) so a daemon that
is already running is still found during the transition. Also add a
`roostctl doctor` check that reports "a session is running at a socket
this context doesn't look at."

### G2. Window size with several clients

**Status: merged: #663 (plan 084).** The
[decision above](#decided-2026-10-28-protocol-7-release) and
[DL-31](../docs/development/vision.md#dl-31-a-tab-has-one-size-owner-2026-10-30)
are what shipped; how it came out is in
[protocol-7-release.md](protocol-7-release.md) § M3. What follows is the
pre-decision analysis, kept as written: "Today's rule" is the rule before
plan 084, and options 1 to 3 became size ownership.

This is the one that decides whether "watch from here, type from there"
*feels* right.

**Today's rule: the last client to interact sets the size** (DL-25,
unchanged by DL-26). The PTY takes the grid of whichever connection
last sent something that carries a window size. Three things in the
shipped client count as that:

1. **Selecting a tab.** The iced client always attaches with
   `focus: true` (`crates/roost-iced/src/app/host_tab.rs:537`), and a
   focused attach resizes the tab before encoding anything
   (`crates/roost-engine/src/attach.rs:205-215`). So *looking* at a tab
   on machine B resizes it to B's window.
2. **Typing or resizing the window** while attached (data-plane
   `INPUT`/`RESIZE`).
3. **The background resize wave, on the slot only.** Under
   `local-backend = session`, the window sends `tab.resize` to every
   tab of its local session that *it* isn't attached to. This happens
   on connect, when a window or sidebar resize settles, on a font
   change, and at the end of an attach
   (`crates/roost-iced/src/app/background_resize.rs`, wired at
   `app.rs:5141-5185`). Its filter only excludes this window's own
   attachments (`tabs_to_resize`), **so it also resizes the tab another
   machine is actively working in.**

**There is no size notification from server to client.** The data plane
has `SNAP`/`PTY`/`EXIT`/`ERROR` from server to client and
`INPUT`/`RESIZE` from client to server (`docs/reference/ipc.md`
data-plane frame table). The event stream has no resize event either.
`AttachAccepted.snapshot_cols/rows` gives the PTY's size once, at
attach. After that, a client that didn't cause a resize isn't told it
happened. **(Unverified, inferred from the above:)** that client keeps
drawing at its own size while the program redraws for another size. A
full-screen TUI then fills only part of a larger window, or wraps and
gets cut off in a smaller one, until that client types and takes the
size back.

**The docs promise more than the client does.** `docs/guides/host-sessions.md:285`
says "a client that only watches (never typing) never shrinks the one
that is working". That's true of the wire protocol: a `focus: false`
attach exists and `tools/roosttest/test_session_attach.py:479` tests
it. But the shipped client never makes a watch-only attach.

**#568 points the other way.** It proposes extending the background
wave to *remote* hosts. For a single client that's right: your tabs
follow your window. For several clients it would add one more machine
that resizes tabs it isn't showing. Settle G2 before landing #568.

**Options. These work together rather than being alternatives:**

1. **Watch first, take the size on interaction.** Selecting a tab
   attaches with `focus: false`, and the client takes the size on its
   first `INPUT` or `RESIZE`. The wire already supports this, so it's a
   client policy change. It makes the guide's sentence true.
2. **Don't background-resize a tab someone else is attached to.** This
   is best decided on the server, which knows the live data connections
   for each tab. Two ways: a `tab.resize` from a connection that isn't
   attached is ignored while another connection is attached; or
   `tab.list` exposes an attachment count so clients can skip those
   tabs. The server-side rule is harder to get wrong.
3. **Tell clients when the size changes, and show a foreign size
   cleanly.** Add a server-to-client geometry signal: either a new
   data-plane frame, which has to be negotiated in the handshake
   because unknown frames are fatal today, or a `tab.resized` event.
   The client then draws the PTY's grid inside its own viewport: padded
   when the window is bigger, clipped or scrollable when it's smaller.
   It shows a small "sized by another client" marker and offers a
   one-key "take size" action. This is the tmux-like experience, and
   it's the closest Roost can get to "looks right on each screen".
4. **A policy setting, later:** `session-geometry = latest | smallest | largest`,
   like tmux's `window-size`. DL-25 rejected smallest-wins as the
   default (a phone glancing at a tab would shrink the desktop), but as
   an opt-in it's cheap once 1–3 exist.

**Leaning:** 1 + 2 + 3 as one piece of work, sequenced before #568.

### G3. Only `localhost` reconnects at launch

Launch-time reconnect only applies to the `localhost` target, and only
connects if a session is already there
(`crates/roost-iced/src/app.rs:505-523`;
`docs/development/host-sessions.md`, "Auto-reconnect never
auto-spawns"). SSH hosts and socket-path hosts are never dialled at
launch. The SSH ladder gives up after 10 attempts. Those are reasonable
defaults for an opt-in feature, where a launch shouldn't silently reach
out over the network or trigger Touch ID prompts. But when the remote
machine *is* where your work lives, clicking Connect on every launch is
friction.

**Options:** a per-host `auto_connect` flag in the host registry, set
from the Add Host dialog (still opt-in, so the outbound-connection
concern stays a user choice). Treat socket-path hosts like `localhost`:
connect if present, since they're local and cheap to try. Possibly an
"keep retrying" setting per host to replace the 10-attempt limit.

### G4. Daemon lifecycle: reboots, upgrades, and which binary

- **Reboot.** Nothing restarts `roost-session`. The guide documents a
  hand-written `systemd --user` unit (Linux, plus `enable-linger`) and a
  launchd LaunchAgent (Mac). If sessions are where everything lives,
  that should ship rather than be a recipe. For example, the package
  could install `/usr/lib/systemd/user/roost-session.service`, enabled
  per user, plus a `roostctl session install-service` for bootstrap
  installs. The package can't start daemons for users by itself.
- **Upgrade means restart, and restart ends every shell.** Every
  `roost-session` restart reopens tabs as fresh shells
  (`docs/guides/host-sessions.md`, upgrade/restart flow). Today that's
  an occasional cost on one host. With sessions everywhere, every
  upgrade on every machine kills running agents. The build-skew `vt`
  fallback (plan 053) helps by letting a newer client connect without
  restarting, but eventually you have to restart. **Speculative:** the
  snapshot encoder already serializes terminal state
  (`ghostty-snapshot` payload). Together with passing PTY master fds
  across an `exec`, that might make a live re-exec handoff possible.
  Neither tmux nor zellij do this, so it would be a real differentiator
  and a large project.
- **Two binaries on one machine.** This box has the apt
  `/usr/bin/roost-session` and the bootstrap's `~/.local/bin/roost-session`.
  The bridge's search order tries `$HOME/.local/bin` first (see the
  `client-bridge` exec in `ps`), so an apt upgrade never reaches the
  daemon the Mac uses. The desktop UI and the daemon then drift apart
  in build (handled by the `vt` fallback or a needs-restart prompt).
  It's worth deciding who owns the daemon binary when a package and a
  bootstrap install both exist: prefer the package copy when it's
  compatible, or don't bootstrap-install over a package at all.

### G5. Where session tabs are worse than in-process tabs

These would make retiring in-process tabs a regression for someone, so
they have to be closed, or explicitly accepted, first:

- Kitty graphics render blank after attach, because the snapshot
  doesn't carry them (`docs/development/host-sessions.md`, "Known
  limitations").
- Effects that fire while nobody is attached (bell, clipboard) are
  gone, by design. Notifications within the replay window are replayed.
- Reduced fidelity on a build-skew `vt` attach (wrapped lines replay as
  separate lines, and so on; `docs/guides/host-sessions.md` "When a
  build skew connects instead").
- Input latency and throughput through the extra socket hop haven't
  been measured against in-process. `roostctl render-stats` and the
  perf bench could do it.
- Each client keeps one attached tab per host. Background tabs update
  from the mirror (title, state, notifications), but their terminal
  content is fetched when you focus them. That's probably fine, but
  it's a visible difference from in-process, where every tab's terminal
  is always live.

### G6. The default and the switch (#480)

Existing setups still default to `in-process` by design (DL-27). #480
tracks the flip and says the remaining work is "the product decision …
not new mechanism". The field report adds prerequisites:

- **G1 has to be fixed first** on Linux. Otherwise, on any machine
  whose session was started from a different context, the flip leads
  to "Roost couldn't start its local session" on every desktop launch.
- **G2's background-wave rule (option 2) should land first.** Otherwise
  flipping a machine to `session` makes its desktop window resize every
  tab that other machines are working in.
- The forward switch copies the in-process layout as *fresh shells*.
  That's fine as a one-time move, but the confirm card should keep
  saying so clearly.

### G7. The Mac

The Swift `Roost.app` has no host sessions and never reads
`local-backend` (DL-27). Sessions everywhere means Roost-Iced on the
Mac, which is where `discovery/swift-retirement.md` and DL-28 already
point. On a Mac the session socket is under `HOME`
(`~/Library/Caches/RoostSession/roost.sock`), so G1 doesn't apply there.

### G8. `roostctl` routing has two-backend special cases

The rules for running `roostctl` inside a session tab
(`docs/reference/cli.md#inside-a-session-tab`) exist because a tab's
`ROOST_SOCKET` can name either a window or a session. They're: ask each
window which session it owns, five verbs stay on the session, and exit
2 if two windows own it. Reaching the window for a *remote* tab is
#560. With sessions only, some of this goes away and some becomes the
main path. It's worth re-reading this section with that end state in
mind rather than patching it piece by piece.

### G9. A tab's environment comes from whoever started the daemon (added 2026-10-27)

**What happens.** A tab's process inherits the daemon's environment.
`tab.open` has no environment field, and the PTY spawn only adds the
`ROOST_*` variables. Nothing scrubs what the daemon inherited, so
whoever started the daemon decides every tab's environment.

**Seen on 2026-10-27.**
- *This Pop!_OS box.* The live daemon was started over Tailscale SSH
  from the Mac. It has `SSH_CLIENT` set and no `DISPLAY`,
  `WAYLAND_DISPLAY` or `XDG_RUNTIME_DIR`. A tab couldn't open a GUI
  window on the machine's own Wayland desktop: a headed browser failed
  with "Missing X server or $DISPLAY".
- *The mac-mini.* Both daemons were started over SSH, and every tab
  carries `SSH_CLIENT`. An SSH-started process there runs in launchd's
  `Background` context, yet the clipboard, `open` and the keychain all
  worked. Only `SSH_AUTH_SOCK` was missing.

**Why it matters.** Tools decide "am I remote?" from `SSH_CONNECTION`.
- Claude Code uses it to choose its clipboard path and whether to open
  a browser. Its copy also always emits OSC 52, so copying reaches the
  viewer either way.
- A forwarded agent socket dies when the SSH connection that started
  the daemon closes.

**Decided:** environment by opener, in
[protocol-7-release.md](protocol-7-release.md) M4.

**Status: shipped in protocol-7 M4 (plan 085).**

## Retiring in-process local tabs (later)

Not proposed now. This section records what it would involve so it can
be weighed when the time comes.

**What would go:** the in-process tab backend in `roost-iced`
(`TabBackend::InProcess`, `crates/roost-iced/src/app/tab_backend.rs`);
the switch, its journal and the launch migration
(`crates/roost-iced/src/app/local_backend.rs`, 5.8k lines including
tests); the `local-backend` key; most of the `HostId::LOCAL`
special-casing (about 133 references in `roost-iced/src`, plus about
152 for `LocalBackendMode`); the in-process-only E2E lanes; and the
two-backend routing in G8.

**What stays:** `roost-engine`'s `Workspace` and PTY supervisor, which
`roost-session` itself runs on. The work moves out of the window
process; it doesn't disappear.

**An intermediate option** is the original HS-5 idea: keep in-process
only as an explicit `--ephemeral` mode (tabs that end with the window,
for throwaway use and tests) rather than as a co-equal backend. That
keeps the escape hatch while dropping the switch and the migration
machinery.

**What should be true first:** G1 fixed, G5 closed or accepted, and a
G4 answer for upgrades. There's no telemetry, so "adoption" has to come
from issues and direct feedback. Decide up front what signal would
count.

## A possible order

*Steps 1 and 2 are now the protocol-7 release
([protocol-7-release.md](protocol-7-release.md)). Step 3's shipped
user service is dropped. Step 4 follows that release.*

1. **Finding the session (G1)**: option A plus B or C, and the `doctor`
   check. Small, low risk, and it unblocks the field-report workflow
   today.
2. **Multi-client window sizes (G2)**: watch-first attach, the
   server-side background-resize rule, then the geometry signal and the
   foreign-size view. Re-evaluate #568 afterwards.
3. **Staying connected (G3, G4 reboot)**: per-host auto-connect and a
   shipped user service.
4. **Flip the default (#480, G6)** once 1 and 2 have landed.
5. **Decide on in-process** (retire, keep `--ephemeral` only, or keep
   as is) based on G5 and feedback. Upgrade handoff (G4) is its own
   track, whenever it's worth the investment.

## Open questions

- **Sidebar labels.** With sessions everywhere, the Pop!_OS desktop
  shows its own session as **LOCAL** while the Mac shows the same
  projects under a **popos** host band. Should bands be named by
  machine everywhere, so the same work has the same name wherever you
  sit?
- **Default size policy.** Is last-interactor (with watch-first) the
  right default, or should a client on the machine itself win over a
  remote one?
- **Is the slot special at all?** If every machine's tabs are on a
  session, does "the slot" stay a concept, or is it just "the host
  whose session is this machine's"?
- **Phones and browsers.** Location independence naturally leads to
  "view from my phone". The wire allows any same-UID client over an SSH
  forward, but "Not a browser host" is a stated non-goal. Does that
  still hold?
- **In-process's fate**: removed, `--ephemeral` only, or kept? And what
  feedback would decide it?
- **Who owns the binary** on a machine with both a package and a
  bootstrap install (G4)?

## Evidence (read-only, 2026-10-22/23)

- `roostctl session status --json` (no `XDG_RUNTIME_DIR`): session
  `9e527086…`, socket `/tmp/roost-session-1000/roost.sock`, 5 projects
  / 9 tabs, `session_protocol` 6, `app_version` 0.0.23.
- `XDG_RUNTIME_DIR=/run/user/1000 roostctl session status`:
  `not-running`, "no session at /run/user/1000/roost-session/roost.sock",
  exit 3.
- `ss -xlp`: listeners at `/tmp/roost-session-1000/roost.sock`
  (`roost-session`, pid 276570) and `/run/user/1000/roost/roost.sock`
  (the desktop `roost`, pid 652672). Nothing at
  `/run/user/1000/roost-session/roost.sock`; that directory holds only
  a `roost.lock` from a refused start.
- `roostctl --socket /run/user/1000/roost/roost.sock host status --json`:
  `localhost` is `disconnected`, with the reason quoted above.
- `/proc/<pid>/environ` for the daemon and the bridges: no
  `XDG_RUNTIME_DIR`. `/proc/652672/environ` (desktop `roost`):
  `XDG_RUNTIME_DIR=/run/user/1000`.
- `~/.local/state/roost/roost.log`: five `localhost session cannot
  start; not retrying … another session (pid 276570) is using this
  state directory` lines at 2026-10-23T01:42Z.
