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

## Adding a remote host (pop-os over SSH)

There's no SSH transport built into Roost yet (that's planned — see the [roadmap](https://github.com/charliek/roost/blob/main/discovery/host-sessions-roadmap.md)), but you don't need one today: a host is just a Unix-domain-socket path, and `ssh -L` can forward one. OpenSSH supports Unix-socket-to-Unix-socket forwarding, so this is zero Roost-side SSH code.

On the remote machine (`pop-os` below), start (or confirm) a session and find its socket:

```bash
ssh pop-os
roostctl session start
echo "$XDG_RUNTIME_DIR/roost-session/roost.sock"   # e.g. /run/user/1000/roost-session/roost.sock
exit
```

Back on your Mac or local Linux box, open a persistent forward — this needs to stay running for the connection to work, so run it in its own terminal (or under something like a systemd user unit or `screen`):

```bash
mkdir -p ~/.roost-hosts
ssh -N -L ~/.roost-hosts/pop-os.sock:/run/user/1000/roost-session/roost.sock pop-os
```

Then add the host in Roost. Either the palette's **Add Host…** dialog (Name: `pop-os`, Socket: `~/.roost-hosts/pop-os.sock`, then **Add & Connect** — which dials `session.identify` before saving, so a typo or a dead forward is caught immediately), or the CLI:

```bash
roostctl host add --label pop-os --target ~/.roost-hosts/pop-os.sock --verify
roostctl host connect --id <the id host add printed>
```

`--verify` is optional on `host add` (registry-only is the default — a typo'd path still saves, and the sidebar's dot reports the problem at the next connect attempt) but matches what the dialog does, so use it when you want the same guarantee from a script.

A remote host is manual-reconnect only: unlike `localhost`, Roost never tries to auto-reconnect it on launch (there's nothing to auto-*start* on a machine you don't own the daemon on). Reconnecting after your Mac sleeps or the SSH tunnel drops is a normal **Connect** — see [Troubleshooting](#troubleshooting) if it doesn't come back.

## The verb set

Every host action lives in the command palette (`Cmd-Shift-P` / `Alt-Shift-P`) — there's no host menu. One row per (verb, host) pair, and a verb only appears where it applies (you can't Stop a session you're not attached to, or Remove one you still are):

| Palette row | What it does |
|---|---|
| **Add Host…** | Opens the Name + Socket dialog described above. Always offered, on every platform. |
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

Roost catches this at Connect, before it ever tries to render anything, and shows the amber "needs restart" dot instead. Connecting again raises a dialog rather than a corrupted screen:

- **On a localhost host you can restart**: *"Restart the session on ‹label›? This session was started by an older/newer Roost (…detail…). Restarting reopens every tab as a fresh shell in its directory — running programs end."* Buttons: **Not now** / **Restart session**. Restarting is `session.stop` → wait for the socket to actually go → spawn a fresh `roost-session` → reconnect, composed on the client side. It's the same layout-survives contract as any other restart — every tab reopens in its saved directory, but whatever was running inside it is gone.
- **On a remote host, this client cannot restart it for you** — restarting is a rung the machine has to climb on its own. The dialog says so instead of offering a dead button: *"The session on ‹label› needs a restart… Only the machine running it can restart it — stop and start the session there (`roostctl session stop`, then `roostctl session start`)."* SSH into that machine and run those two commands yourself, then Connect again from here.

## macOS note

There is no `roost-session` build for macOS yet (tracked as future work — see the [roadmap](https://github.com/charliek/roost/blob/main/discovery/host-sessions-roadmap.md)). On the Roost-Iced Mac build this means:

- The `localhost` surface is hidden entirely: no seeded `Connect Host: localhost` row, no launch-time auto-reconnect, nothing that would spawn a session your Mac can't build.
- **Add Host still works** — pointing your Mac at an `ssh -L` forward to a Linux box (the [remote-host recipe](#adding-a-remote-host-pop-os-over-ssh) above) is the whole Mac→Linux payoff of this feature, not a dead end. Everything past Add Host — Connect, the sidebar section, the verb set — behaves identically once you've added a remote target.

## Troubleshooting

**"Needs restart" / an amber dot that won't turn green.** A build or protocol mismatch — see [The upgrade / restart flow](#the-upgrade-restart-flow) above. This is expected the first time you Connect after upgrading Roost on a machine whose `roost-session` predates the upgrade.

**A host stays "disconnected" and won't come back (a stale socket).** The socket file can outlive the process that created it — the daemon crashed, was killed, or the machine rebooted without cleaning up. On `localhost`, Roost retries the connection itself with a capped backoff (you'll see the amber dot briefly, then grey); if it never recovers, run `roostctl session status` (or, over SSH, run it on the remote machine) to check whether anything is actually listening, and `roostctl session start` if not. On a remote host reached over an `ssh -L` forward, first check the tunnel itself is still up — a dropped SSH connection looks identical to a dead session from Roost's side.

**"The session on ‹label› ended."** This is the honest reading of a session that told Roost it was shutting down cleanly (an explicit `session.stop`, from any client — including a palette Stop from a different window). The banner's button is **Start a new session**, not a reconnect — the previous session's shells are genuinely gone, and this starts a fresh one. If you didn't stop it yourself, check whether another client or script did (`roostctl session stop` from anywhere reaches the same daemon).

**A host tab's own attention doesn't reach you.** If you're actively attached to and looking at a tab on a host, and that same tab is also the thing generating a notification (a Claude Code hook firing, a bell, a build finishing), the notification currently doesn't reach this client — the desktop banner and inbox row for that one tab are silently suppressed. This is a known gap: `roost-session` runs headless and its internal workspace always considers itself "focused" (there's no window to report otherwise, and no op yet pushes an attached client's real focus state to it), so the same-tab suppression rule regular Roost notifications rely on (see [Notifications → Focus policy](notifications.md#focus-policy)) ends up permanently true for that tab. Every *other* tab on the host is unaffected. Tracked as follow-up work (HS-3).

## Related

- [`reference/ipc.md`](../reference/ipc.md#session-sockets) — the wire contract this feature drives, including the `host.*` ops and the attach data plane.
- [`reference/cli.md`](../reference/cli.md#session-subcommands) — `roostctl session` and `roostctl host` in full.
- [Host sessions (development)](../development/host-sessions.md) — the architecture: how `HostConn` is put together, the attach sequence, and the lease/takeover lifecycle.
- [Keybindings](../getting-started/keybindings.md) — the full shortcut table, including the host-context notes on `Cmd-N`/`Cmd-T`.
