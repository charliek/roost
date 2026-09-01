# Host Sessions

A host session is a workspace that keeps running after Roost's window closes. Instead of a PTY supervisor living inside the Roost app itself, it lives inside a small headless daemon — `roost-session` — and Roost attaches to it the way you'd attach to a `tmux` server, except the chrome is the real Roost sidebar and tabs, not a text UI. Close the window, and the shells inside it — including whatever long-running agent you had going — keep going. Reopen Roost, and they're still there.

This guide is task-shaped: enabling a host on your own machine, reaching a second machine over SSH, the verb set, and what to do when something looks wrong. For the wire contract, see [`reference/ipc.md`](../reference/ipc.md#session-sockets); for the architecture, see [Host sessions (development)](../development/host-sessions.md).

**Linux only, for now.** Host sessions are built into the iced UI — the Linux `roost` package and the experimental Roost-Iced Mac app. There is no `roost-session` daemon for macOS yet, so a Mac can only be a *client* of a host running elsewhere — see [macOS note](#macos-note) below. The Swift `Roost.app` doesn't have this feature at all; host sessions are iced-only.

## Enabling a host on this machine (localhost)

If `roost-session` isn't running yet, the fastest way to get one is to just try connecting — the palette will offer to start it for you:

1. Open the command palette (`Cmd-Shift-P` / `Alt-Shift-P`).
2. On a fresh install with no saved hosts, you'll see **Connect Host: localhost**. Activate it.

That saves `localhost` as a host and connects it in one step, starting `roost-session` if nothing is listening yet. You can also do this by hand:

```bash
roostctl session start   # starts (or confirms) roost-session
```

then use the palette's **Add Host…** (or `roostctl host add`, below) to save it.

Once connected, the sidebar gains a **LOCALHOST** section (or whatever label you gave it) alongside your local **LOCAL** projects, and any project you create there survives quitting Roost. With zero hosts saved, the sidebar is exactly what it always was — this is purely additive.

## Adding a remote host (over SSH)

Roost reaches a remote host directly over `ssh` — there's nothing to forward by hand first. A saved host's `target` can be any of:

- a bare hostname or `user@host` (`workbox`, `charlie@workbox`) — anything `ssh(1)` itself would accept as a destination, resolved through your own `~/.ssh/config` (host aliases, `IdentityFile`, `ProxyJump`, all of it apply);
- `ssh://user@host:port` — the only spelling that carries an explicit port (`ssh://workbox:2222`). A bare `host:port` with no `ssh://` scheme is rejected — it reads ambiguously against a Unix socket path — and a literal IPv6 address needs brackets: `ssh://[::1]:22`;
- a Unix socket path — anything containing `/` (`/tmp/roost.sock`, `~/.roost-hosts/workbox.sock`, `./roost.sock`). This is the pre-SSH-transport form and still works unchanged — see [Scripted / fallback forwarding](#scripted-fallback-forwarding-with-ssh-n-l) below for when you'd still reach for it;
- `localhost` — this machine's own session, unchanged.

Target strings are trimmed before they're classified, and a target starting with `-` is refused outright (it would otherwise reach `ssh` as a flag rather than a destination). One behavior change from earlier builds: a bare relative filename with no `/` in it — `roost.sock` sitting in the current directory — used to be read as a same-directory socket path. It's now read as an SSH hostname instead, since there's no way to tell the two apart without a separator; spell a same-directory socket path `./roost.sock`.

On the remote machine (`workbox` below), start (or confirm) a session once:

```bash
ssh workbox
roostctl session start
exit
```

This manual step is optional, not required — skip it and just Add & Connect below, and if nothing is there yet Roost detects that and offers to install and start `roost-session` for you in-app (see [Troubleshooting](#troubleshooting)'s not-found and no-session rows). Doing it by hand here is still the fastest path if you're already `ssh`ed in for another reason.

Then add the host — no forward to stand up first. Either the palette's **Add Host…** dialog (Name: `workbox`, Target: `workbox`, then **Add & Connect** — which dials the target before saving, so a typo or an unreachable host is caught immediately), or the CLI:

```bash
roostctl host add --label workbox --target workbox --verify
roostctl host connect --id <the id host add printed>
```

`--verify` is optional on `host add` (registry-only is the default — a typo'd target still saves, and the sidebar's dot reports the problem at the next connect attempt) but matches what the dialog does. Over an SSH target it's a mux-less one-shot probe — its own throwaway `ssh` exec, outside any multiplexed connection, nothing left running afterward — so it's safe to use from a script or a dialog before you've committed to the host.

A remote host is manual-reconnect only: unlike `localhost`, Roost never tries to auto-reconnect it on launch or after a drop (there's nothing to auto-*start* on a machine you don't own the daemon on). Reconnecting after your Mac sleeps or the SSH connection drops is a normal **Connect** — see [Troubleshooting](#troubleshooting) below for what a failed SSH connect looks like and how to recover.

### Scripted / fallback forwarding with `ssh -N -L`

Roost's own SSH transport execs a fresh `ssh -T … 'exec roost-session client-bridge'` per connection over a private multiplexed `ssh` master — see [Host sessions (development) → Transport: SSH hosts](../development/host-sessions.md#transport-ssh-hosts) for the shape. It is not a socket forward, but the older forwarding recipe below still works, because a saved host can always be a plain socket path: OpenSSH supports Unix-socket-to-Unix-socket forwarding, so this needs zero Roost-side SSH code and stays useful for a script, a `systemd --user` unit, or a host you'd rather reach through an already-open tunnel than hand Roost an interactive login.

On the remote machine, start (or confirm) a session and find its socket:

```bash
ssh workbox
roostctl session start
echo "$XDG_RUNTIME_DIR/roost-session/roost.sock"   # e.g. /run/user/1000/roost-session/roost.sock
exit
```

Back on your Mac or local Linux box, open a persistent forward — this needs to stay running for the connection to work, so run it in its own terminal (or under something like a systemd user unit or `screen`):

```bash
mkdir -p ~/.roost-hosts
# Use the socket path the command above printed — the `1000` in
# `/run/user/1000` is that machine's UID, not necessarily yours.
ssh -N -L ~/.roost-hosts/workbox.sock:/run/user/1000/roost-session/roost.sock workbox
```

Then add the forwarded socket as a host — a socket-path target, exactly like the native form above but pointed at the local end of the tunnel:

```bash
roostctl host add --label workbox --target ~/.roost-hosts/workbox.sock --verify
roostctl host connect --id <the id host add printed>
```

## The verb set

Every host action lives in the command palette (`Cmd-Shift-P` / `Alt-Shift-P`) — there's no host menu. One row per (verb, host) pair, and a verb only appears where it applies (you can't Stop a session you're not attached to, or Remove one you still are):

| Palette row | What it does |
|---|---|
| **Add Host…** | Opens the Name + Target dialog described above. Always offered, on every platform. |
| **Connect Host: `<label>`** | Dials the host, starting it first if it's `localhost` and nothing answers. Reconnecting to an already-connected host is a deliberate takeover (see below). |
| **Disconnect Host: `<label>`** | Drops the connection. The session's shells keep running. |
| **Stop Session: `<label>`** | Ends every shell on that host and flushes its layout. Offered only while connected — you can't stop what you're not attached to. Confirms first. |
| **Remove Host: `<label>`** | Forgets the saved host. Offered only while disconnected (removing a live connection would race it). Never touches the session itself. |
| **New Project on…** | Opens a picker of LOCAL plus every *connected* host, and creates the new project there. Appears once you have at least one saved host. |

`roostctl` has a matching verb for every one of these except Stop (which is a plain `session.stop` on the host's own connection, not a client-side registry op):

```bash
roostctl host add --label pop-os --target ~/.roost-hosts/pop-os.sock
roostctl host list
roostctl host connect --id <id>
roostctl host disconnect --id <id>
roostctl host remove --id <id>
```

See [`reference/cli.md`](../reference/cli.md#host-subcommands) for the full flag reference.

### Keybinds and creation routing

Creation follows context, so you never have to think about "which host am I on" for the common case:

| Shortcut | What it creates |
|---|---|
| `Cmd-N` / `Alt-N`, or the sidebar's **+ New Project** button | A new project on the **currently selected project's host** (LOCAL if you're not looking at a host). |
| `Cmd-Shift-N` / `Alt-Shift-N` | Opens **New Project on…** — pick LOCAL or any connected host explicitly. |
| `Cmd-T` / `Alt-T`, or the tab bar's **+** | A new tab on the **project's own host** — a tab can never land on a different host than its project. |

`Cmd-Shift-A` / `Alt-Shift-A` (toggle sidebar agent rows) and the agents palette (`Cmd-Shift-O` / `Alt-Shift-O`) both span every host — a running Claude session on `pop-os` shows up next to a local one, suffixed `project · pop-os` in the palette. See [Keybindings](../getting-started/keybindings.md) for the complete table.

## Disconnect vs. Stop

These are deliberately different actions:

- **Disconnect** (closing the Roost window, quitting, or the palette's Disconnect verb) leaves the session's shells running. The sidebar keeps listing that host's projects and tabs, dimmed, with an inline **↻ Reconnect** row underneath — those shells are still alive on the host, so the sidebar says so rather than pretending they're gone.
- **Stop Session** actually ends them — every PTY is hung up, the layout is flushed, and reconnecting after a Stop starts every tab over as a fresh shell in its saved directory (same "layout, not live state" contract normal Roost restarts use).

If all you want is to close your laptop for the night, disconnect (or just quit) — don't stop.

## Takeover

A session holds one interactive lease at a time. If you connect to the same host from a second window (a second machine, or the same machine after a crash left the first window's connection stale), the new connection **takes over**: it gets the lease, and the *displaced* window is told.

The displaced window keeps its last frame on screen — frozen, dimmed — under a banner:

> **‹label› was taken over by another Roost window.** [Reconnect here]

"Reconnect here" is an ordinary Connect: it takes the lease back. There's no data loss either way — the shells themselves don't care who's watching; only the interactive connection moves.

## The upgrade / restart flow

`roost-session` and the Roost client both pin the same libghostty build, and every tab's snapshot depends on the two agreeing exactly. The most common way this feature breaks — on literally every package upgrade, until the host itself is restarted — is a build (or protocol) mismatch: you upgrade Roost, but a `roost-session` you started before the upgrade is still running the old build.

Roost catches this at Connect, before it ever tries to render anything, and shows the amber "needs restart" dot instead. Connecting again raises a dialog rather than a corrupted screen, and which one depends on how the host is reached:

- **On a localhost host you can restart**: *"Restart the session on ‹label›? This session was started by an older/newer Roost (…detail…). Restarting reopens every tab as a fresh shell in its directory — running programs end."* Buttons: **Not now** / **Restart session**. Restarting is `session.stop` → wait for the socket to actually go → spawn a fresh `roost-session` → reconnect, composed on the client side. It's the same layout-survives contract as any other restart — every tab reopens in its saved directory, but whatever was running inside it is gone.
- **On a remote host reached over SSH, Roost offers to fix it for you.** *"The session on ‹label› needs a restart… Roost can install the matching roost-session on ‹label› over ssh and restart the session there — it will show you what it would do before anything is changed."* Button: **Update roost-session on ‹label›**. This runs the same install/upgrade flow as the [not-found row](#troubleshooting) below — a consent card naming exactly what will be installed and from where, shown before anything is touched — then installs the matching build, stops the stale session, waits for it to actually go, starts the new one, and reconnects. If the session on the far end happens to be *newer* than this Roost, the dialog says so instead — installing an older build isn't the fix there, upgrading Roost is.
- **On a remote host reached over the [fallback `ssh -L` forward](#scripted-fallback-forwarding-with-ssh-n-l) (a plain Unix-socket target), this client still can't reach in and fix it for you** — the bootstrap flow only knows how to reach a host over Roost's own SSH transport. The dialog says so instead of offering a dead button: *"The session on ‹label› needs a restart… Only the machine running it can restart it — stop and start the session there (`roostctl session stop`, then `roostctl session start`)."* SSH into that machine and run those two commands yourself, then Connect again from here.

**Roost never installs or restarts anything without asking first**, on every one of these paths — the consent card is always the first remote activity, and Cancel leaves the host exactly as it was. `roostctl` has no install, upgrade, or remote-restart verb of its own, and an IPC-originated connect (a script, a hook) never raises a dialog — a machine is never prompted, only a person looking at the window is.

## macOS note

There is no `roost-session` build for macOS yet (tracked as future work — see the [roadmap](https://github.com/charliek/roost/blob/main/discovery/host-sessions-roadmap.md)). On the Roost-Iced Mac build this means:

- The `localhost` surface is hidden entirely: no seeded `Connect Host: localhost` row, no launch-time auto-reconnect, nothing that would spawn a session your Mac can't build.
- **Add Host still works, and it's the whole Mac→Linux payoff of this feature.** Point your Mac straight at a Linux box's SSH target — `workbox` / `user@host` / `ssh://…` — and Add Host reaches it directly; there's no manual `ssh -L` forward to stand up first (see [Adding a remote host](#adding-a-remote-host-over-ssh) above — the older forwarding recipe still works too, if you'd rather use it). Everything past Add Host — Connect, the sidebar section, the verb set — behaves identically once you've added a remote target.

## Troubleshooting

**"Needs restart" / an amber dot that won't turn green.** A build or protocol mismatch — see [The upgrade / restart flow](#the-upgrade-restart-flow) above. This is expected the first time you Connect after upgrading Roost on a machine whose `roost-session` predates the upgrade.

**A host stays "disconnected" and won't come back (a stale socket).** The socket file can outlive the process that created it — the daemon crashed, was killed, or the machine rebooted without cleaning up. On `localhost`, Roost retries the connection itself with a capped backoff (you'll see the amber dot briefly, then grey); if it never recovers, run `roostctl session status` (or, over SSH, run it on the remote machine) to check whether anything is actually listening, and `roostctl session start` if not. On a remote host reached over the [fallback `ssh -L` forward](#scripted-fallback-forwarding-with-ssh-n-l), first check the tunnel itself is still up — a dropped SSH connection looks identical to a dead session from Roost's side.

**Connecting over a native SSH target fails outright (a classified reason).** Every SSH connect attempt is bounded, and a failure is classified into one of six families — its exact copy lands in the sidebar band as `disconnected — <reason>`, and in the log; an attempt you asked for (a palette Connect, `roostctl host connect`) also raises a toast, a background retry does not. The families, and what they mean:

| Family | Copy (target interpolated) | What to do |
|---|---|---|
| Changed host key | *"the host key for `<target>` has CHANGED since it was last seen — this can mean the host was reinstalled, or that something is impersonating it. Do not accept the new key from here; verify its fingerprint with `<target>` out-of-band…"* | Never dismiss this by accepting the new key from inside Roost — there's no prompt to accept it with, on purpose. Verify the new fingerprint some other way (a call, a channel other than this machine), then update it yourself with `ssh-keygen -R <target>` and `ssh <target>` once, out-of-band, before connecting again. |
| Unknown host key | *"`<target>`'s host key has not been seen before. Run `ssh <target>` once in a terminal to review and accept it, then try again."* | Do exactly that — Roost's SSH transport runs with `BatchMode=yes`, so it can never prompt to accept a key itself. |
| Auth refused | *"`<target>` refused authentication. Check that your key is loaded in an agent, then try `ssh <target>` in a terminal to confirm you can log in."* | `BatchMode=yes` also means **no password and no interactive 2FA prompt** — key/agent auth only, this slice. Confirm `ssh <target>` logs in on its own first. |
| No session on the far end | *"`<target>` is reachable but has no roost session running. Run `roostctl session start` on that machine, then try again."* | Exactly what it says — SSH itself is fine, there's just nothing to attach to yet. From the palette or the Add Host dialog (an *attended* connect), Roost also offers to start it for you in-app — a **Start roost-session on ‹label›?** consent card, no writes beyond starting the process you already have installed. `roostctl host connect` / `host add --verify` still just report this and stop — a script is never prompted. |
| `roost-session` not found | *"roost-session isn't installed on `<target>` (or isn't on the non-interactive PATH ssh uses there) — connect from the Roost app to install it."* | **The #1 gotcha in practice — and Roost now offers to fix it for you.** From the palette or the Add Host dialog, a NotFound result opens an **Install roost-session on ‹label›?** consent card naming exactly what will be installed and from where (this Roost's own binary, a checksum-verified download, or an override you've set) before anything is written; confirming installs it to `~/.local/bin`, starts it, and reconnects. Cancel touches nothing. Roost execs the remote command as `sh -c '... exec roost-session client-bridge'` — a non-interactive, non-login shell, so anything a login-only rc file (`.bash_profile`, `.zprofile`) or an interactive-only guard adds to `PATH` is invisible to it, which is exactly why the install lands in `~/.local/bin`: it's the first place Roost looks, so it sidesteps the whole PATH question on the very next connect. `roostctl` has no install verb of its own and never prompts — this offer only ever appears in the app, to a person looking at it. |
| Transport (anything else) | *"connecting to `<target>` failed: `<last stderr line>`"* (or with no line, just *"connecting to `<target>` failed"*) | The catch-all — a DNS failure, a refused TCP connection, a timeout. The interpolated line is `ssh`'s own last line of stderr; read it literally. |

Two things that look like a hang but aren't: a biometric-gated SSH agent (Touch ID/agent unlock) can eat a few seconds of the connect budget on the first use in a while — the warm-up connection gets 30 seconds (scaled up under `ROOST_TEST_TIMEOUT_SCALE`) before it's classified as a transport timeout, which is generous for a single unlock prompt but not infinite; and `BatchMode=yes` means there's genuinely no password/2FA prompt to answer, ever, over this transport — if a host requires one, log in with a plain `ssh <target>` once to confirm your key/agent setup covers it non-interactively, or fall back to the [`ssh -L` forward](#scripted-fallback-forwarding-with-ssh-n-l) recipe if it can't.

**"The session on ‹label› ended."** This is the honest reading of a session that told Roost it was shutting down cleanly (an explicit `session.stop`, from any client — including a palette Stop from a different window). The banner's button is **Start a new session**, not a reconnect — the previous session's shells are genuinely gone, and this starts a fresh one. If you didn't stop it yourself, check whether another client or script did (`roostctl session stop` from anywhere reaches the same daemon).

**A host tab's own attention doesn't reach you (older sessions only).** Roost now tells a host session what you are actually looking at — which tab is selected, and whether the window has focus — so the same-tab suppression rule ([Notifications → Focus policy](notifications.md#focus-policy)) applies to the tab you are really watching, exactly as it does locally. Against a `roost-session` older than this release there is nothing to tell: that session runs headless and always considers itself "focused" on whichever tab it thinks is active, so notifications for that one tab are silently suppressed (every *other* tab on the host is unaffected). Upgrading `roost-session` on the host and reconnecting fixes it.

## Related

- [`reference/ipc.md`](../reference/ipc.md#session-sockets) — the wire contract this feature drives, including the `host.*` ops and the attach data plane.
- [`reference/cli.md`](../reference/cli.md#session-subcommands) — `roostctl session` and `roostctl host` in full.
- [Host sessions (development)](../development/host-sessions.md) — the architecture: how `HostConn` is put together, the attach sequence, and the lease/takeover lifecycle.
- [Keybindings](../getting-started/keybindings.md) — the full shortcut table, including the host-context notes on `Cmd-N`/`Cmd-T`.
