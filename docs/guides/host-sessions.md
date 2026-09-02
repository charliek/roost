# Host Sessions

A host session is a workspace that keeps running after Roost's window closes. Instead of a PTY supervisor living inside the Roost app itself, it lives inside a small headless daemon — `roost-session` — and Roost attaches to it the way you'd attach to a `tmux` server, except the chrome is the real Roost sidebar and tabs, not a text UI. Close the window, and the shells inside it — including whatever long-running agent you had going — keep going. Reopen Roost, and they're still there.

This guide is task-shaped: enabling a host on your own machine, reaching a second machine over SSH, the verb set, and what to do when something looks wrong. For the wire contract, see [`reference/ipc.md`](../reference/ipc.md#session-sockets); for the architecture, see [Host sessions (development)](../development/host-sessions.md).

**Both platforms run a local session now.** Host sessions are built into the iced UI — the Linux `roost` package and the Roost-Iced Mac app both ship `roost-session` and can run a `localhost` session of their own; see [Enabling a host on this machine](#enabling-a-host-on-this-machine-localhost) below. What's still Linux-only is being an SSH *host*: reaching a machine as a remote SSH target still means a Linux box on the far end — see [macOS note](#macos-note) below for what that means for a Mac. The Swift `Roost.app` doesn't have this feature at all; host sessions are iced-only.

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

`roost-session`'s own socket, state, and logs live at: on Linux, `$XDG_RUNTIME_DIR/roost-session/roost.sock` (state under `$XDG_DATA_HOME/roost-session/`, logs under `$XDG_STATE_HOME/roost-session/`); on macOS, `~/Library/Caches/RoostSession/roost.sock` (state under `~/Library/Application Support/RoostSession/`, logs under `~/Library/Logs/RoostSession/` — `RoostSessionDev` in place of `RoostSession` for a debug build, so a dev daemon can never collide with a real one). See [Paths and Environment](../reference/paths.md#session-profile) for the full table.

**Persistence is scoped to host sections, not to Roost as a whole — worth stating plainly.** A project or tab created under *any* host section, including **LOCALHOST**, survives quitting Roost, because its shells live in `roost-session`, not in the app. An ordinary **LOCAL** tab is still hosted in-process and still ends when Roost quits, exactly as before — nothing about this feature changes that. Making persistence the default for local tabs too is a real option, but it's a deliberate future decision, not something this slice changed quietly.

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

A remote host never auto-connects at launch — Roost doesn't even reach for it, so it simply sits disconnected until you Connect. That's deliberate: connecting to a remote machine is an outbound decision that costs a handshake and can fail loudly, and when Roost has only just opened, nobody has asked for it. Once you've connected it, though, a remote host behaves like `localhost` again for a *mid-session* drop: close your laptop, let Wi-Fi hiccup, whatever kills the link — Roost notices and retries on its own, with a growing delay (up to 30 seconds) shown right in the sidebar band as `reconnecting in Ns (k/10)`. If ten attempts don't get it back it settles with `reconnect gave up after 10 tries`, and ↻ Reconnect — which was on screen the whole time — is exactly the button you'd have clicked anyway. Not every failure gets retried this way: a changed or unknown host key, a rejected login, and a session that's genuinely gone each settle immediately instead, because each of those needs a person to do something different — see [Troubleshooting](#troubleshooting) below for what a failed SSH connect looks like and how to recover.

A rebooted remote machine is the one case that never grows a ladder: it comes back with no `roost-session` running at all, so the very first reconnect attempt classifies as "no session" and settles right away (see the [no-session row](#troubleshooting) below) rather than retrying. If you want a host to survive its own reboots without you having to reconnect by hand, run `roost-session` as a `systemd --user` unit and enable lingering for that user — `loginctl enable-linger <user>` — so the unit starts at boot instead of waiting for a login. With that in place the session is already up by the time Roost's client retries, and auto-reconnect covers the rest.

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

`localhost` now works on macOS exactly the same way it does on Linux: `roost-session` ships inside `Roost-Iced.app` itself (`Contents/MacOS/roost-session`), so the seeded **Connect Host: localhost** row, spawn-if-missing, and launch-time auto-reconnect described above all apply unchanged to the Mac build. What stays Mac-specific:

- **No SSH-to-a-Mac bootstrap yet.** Reaching a remote machine over SSH still means a Linux box on the far end: the install/upgrade flow that probes a remote and offers to install or start `roost-session` there for you (the [not-found row](#troubleshooting) below, and the remote "needs restart" dialog) only knows how to target Linux, so a Mac can't be added as a remote SSH host through those flows today. That's tracked as future work — see the [roadmap](https://github.com/charliek/roost/blob/main/discovery/host-sessions-roadmap.md).
- **Add Host still reaches a Linux box directly** — `workbox` / `user@host` / `ssh://…` — with no manual `ssh -L` forward to stand up first (see [Adding a remote host](#adding-a-remote-host-over-ssh) above — the older forwarding recipe still works too, if you'd rather use it). Everything past Add Host — Connect, the sidebar section, the verb set — behaves identically whether the target is `localhost` or a remote Linux machine.
- **The Swift `Roost.app` still has none of this feature at all** — permanently (see the top of this guide).

### Surviving reboots (launchd)

The [SSH-host reboot recipe](#adding-a-remote-host-over-ssh) above — `systemd --user` plus `loginctl enable-linger` — is Linux-specific. A Mac's own `roost-session` needs a different supervisor to come back on its own after a reboot, and the mechanism macOS actually offers has different scope, so this states the trade honestly rather than pretending it's the same trick.

A `launchd` LaunchAgent is the macOS analog: a plist under `~/Library/LaunchAgents/` that launchd loads for your login session and can be told to keep the process running.

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>ai.stridelabs.roost-session</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/Roost-Iced.app/Contents/MacOS/roost-session</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
</dict>
</plist>
```

`--foreground` matters under a supervisor: launchd wants to own the process directly, so the daemon must not fork away from it the way an unsupervised `roost-session start` normally does.

`KeepAlive` is deliberately `{SuccessfulExit: false}`, not bare `true`. A clean **Stop Session** (or `roostctl session stop`) exits `0` — under bare `KeepAlive: true`, launchd would read that as a crash and immediately resurrect the daemon you just told to stop, and it would just as happily race the upgrade flow's own stop-then-restart. `SuccessfulExit: false` reads that same exit `0` as "stopped on purpose" and only respawns on a nonzero exit — an actual crash — which auto-reconnect's connect-if-present then absorbs on its own.

Load it once with:

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.stridelabs.roost-session.plist
```

To stop it for good — not just for now — unload the agent:

```bash
launchctl bootout gui/$(id -u)/ai.stridelabs.roost-session
```

**Stop Session** and `roostctl session stop` still work exactly as they always do under supervision: the daemon ends and, because of `SuccessfulExit: false`, stays ended. What it does *not* do is forget the LaunchAgent — `RunAtLoad` starts a fresh session at your next login. Boot it out when you want that to stop happening too.

Say what this actually buys you, honestly: a `gui/$UID` LaunchAgent starts at your **next login**, not at boot, and it stops at **logout** — this is not the same scope as Linux's `enable-linger` (which keeps a unit running with nobody logged in at all). A reboot brings your saved sidebar layout back either way, the next time Roost opens, but the *running shell processes* inside a session never survive a reboot, on any platform — what the LaunchAgent buys you is not having to remember to start `roost-session` by hand after you log back in.

Point this at your real install (`/Applications/Roost-Iced.app`), not a dev/debug build — a debug build resolves the separate `RoostSessionDev` profile (see [Paths and Environment](../reference/paths.md#session-profile)), so pointing the plist at one is harmless, just confusing to debug later.

## Troubleshooting

**"Needs restart" / an amber dot that won't turn green.** A build or protocol mismatch — see [The upgrade / restart flow](#the-upgrade-restart-flow) above. This is expected the first time you Connect after upgrading Roost on a machine whose `roost-session` predates the upgrade.

**A host stays "disconnected" and won't come back (a stale socket).** The socket file can outlive the process that created it — the daemon crashed, was killed, or the machine rebooted without cleaning up. On `localhost`, Roost retries the connection itself with a capped backoff (you'll see the amber dot briefly, then grey); if it never recovers, run `roostctl session status` (or, over SSH, run it on the remote machine) to check whether anything is actually listening, and `roostctl session start` if not. A host reached over Roost's own SSH transport does the same once it's actually connected at least once — a mid-session drop retries on its own (`reconnecting in Ns (k/10)`, capped at 30 seconds between tries) and settles with a "gave up after 10 tries" message if the far side genuinely isn't coming back; ↻ Reconnect is still right there either way. On a remote host reached over the [fallback `ssh -L` forward](#scripted-fallback-forwarding-with-ssh-n-l) instead — a plain socket path, not Roost's SSH transport — none of that applies: first check the tunnel itself is still up, since a dropped SSH connection there looks identical to a dead session from Roost's side.

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
