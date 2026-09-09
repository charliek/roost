# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/). Releases are
cut with `/release-workflows:release vX.Y.Z` — it curates the section below, commits, tags
`vX.Y.Z`, and pushes; the tag triggers `.github/workflows/release.yml`, which
builds the DMG + `.deb`s and publishes to the apt repo. Bump
`[workspace.package].version` in `Cargo.toml` to match before tagging (the
release workflow asserts they agree).

## Unreleased

### Added

- **A libghostty build skew connects instead of demanding a restart
  (#420)** — a host session and the Roost connecting to it used to have
  to pin the *same* libghostty build, because the only attach payload
  was libghostty's own binary snapshot. Upgrade Roost while a
  `roost-session` from before the upgrade is still running and the host
  went amber, listed no tabs, and offered one remedy: restart the
  session, ending every shell on it. There is now a second payload
  kind, **`vt`** — a plain VT byte stream the client replays into its
  own terminal, so no build has to match — and a session serves it
  beside `ghostty-snapshot`, picked automatically when the binary
  format cannot be. A skewed host therefore just **connects**: no dot,
  no dialog, tabs render, you type into them. `ghostty-snapshot` stays
  first in every preference list, so nothing changes when the builds
  agree. Amber "needs restart" now means the narrower pair of things it
  should — a *protocol* mismatch, or a session too old to serve `vt` at
  all. It is honestly a lower-fidelity connection, and the tradeoffs
  are worth reading before relying on it: link hover and click stop
  working on such a tab, attaching over a full-screen program leaves
  the shell underneath blank when it exits, wrapped lines replay as
  separate lines, and a coloured status bar that has scrolled into
  history loses its fill. `roostctl host status --json` reports
  `payload_kind` once a tab has attached, which is the only way to tell
  from outside. One gap stated plainly: the "Update roost-session on
  ‹host›" offer is raised only from the state a fallback now avoids, so
  **a remote SSH host on `vt` has no in-app path to update its
  daemon** — update the binary over ssh and restart the session there
  by hand, or `roostctl session stop` over ssh and Connect again to get
  the ordinary install offer, until an on-demand action lands (#447). See the
  [host sessions guide](docs/guides/host-sessions.md#the-upgrade-restart-flow)
  for the full list and the recovery routes, and
  [ipc.md](docs/reference/ipc.md#payload-kinds) for the wire contract.
- **`tab.dump` can return scrollback, not just the viewport (#421)** —
  the op every test, script and `roostctl tab dump` reads a terminal
  through has always answered with the visible grid alone, so anything
  that scrolled off was unreachable even though all three terminals
  retain 2000 rows of it. It now takes an optional `scrollback` row
  count and answers with `scrollback_rows` (how much history exists
  above what it is showing) plus `scrollback_text` (that many rows of
  it). `roostctl tab dump --tab 5 --scrollback 200` prints the history
  and then the viewport with no separator between them, so an existing
  `| grep` keeps working over a much larger window. The history is
  anchored on the **viewport that was dumped**, so its last row is
  always the one directly above the first visible row — true on a tab
  the user has scrolled up, not just at the live bottom. Asking for
  more than exists is clamped rather than refused, which makes
  `--scrollback 1000000` a legitimate way to say "all of it". Served
  identically on a host session's socket, a UI socket, and by both the
  Linux and macOS UIs; a session advertises it as `tab_dump_scrollback`
  in `session.identify.features`. No protocol bump — but the request
  key is new, so a Roost or session predating it answers `unknown-field`
  to the flag rather than quietly ignoring it.
- **`server_url` metadata key makes an opencode session drivable (#439)** —
  a bare `opencode` binds no socket (its TUI talks to its server
  in-process), so nothing could act on the session roost already
  reported. Roost's opencode plugin runs inside that server process, so
  it now puts a loopback listener (`127.0.0.1`, port 0) in front of the
  in-process app and reports the address as `metadata["server_url"]` on
  the owning tab; when opencode is already listening itself (`serve`,
  `web`, `--port`, and the rest) it reports that address and serves
  nothing — the two are told apart by whether the client opencode hands
  the plugin carries an in-process `fetch`, which opencode supplies only
  when there is no real listener, not by parsing the command line. The
  value is recorded only when it parses as a
  token-free loopback base URL, and the listener demands the same Basic
  credentials opencode does when `OPENCODE_SERVER_PASSWORD` is set.
  `ROOST_OPENCODE_NO_SERVER=1` opts out of the listener roost creates —
  it does not suppress reporting a server you started yourself.
  `roostctl doctor` prints the value rather than its length, because a
  loopback URL is not a secret and printing it is what makes the
  handshake debuggable. No protocol bump — `metadata` is the no-bump
  extension channel. Note the widened surface (a bare
  `opencode` in a roost tab is now reachable by any same-host process)
  and the mid-session limitation (the key rides *every* forwarded event
  and merges onto ownership the tab already has, so a plugin that loads
  mid-session only has to wait when the tab is **unowned** — with no
  claim to ride and nothing to merge onto, the key lands at the next
  `session.created`); both are in the
  [Agent Hooks](docs/guides/agents.md#opencode-keys) guide.

- **`events.subscribe` can resume instead of re-snapshotting (#422)** — a
  phone dropping in and out of a flaky link used to pay a `tab.list`
  snapshot and a re-fence on every stall; a host session now keeps a
  bounded ring of committed batches behind its revision fence, and
  `events.subscribe` takes two new optional request fields,
  `from_revision` (what the client already has) and `session_id` (the
  incarnation it fenced against — required alongside `from_revision`,
  since revisions restart per process and an unnamed fence could
  otherwise be served a different session's history that still passes
  the client's own gap check). A resume outside the retained window
  answers `replay-expired` naming how far back it can still go; ahead of
  the session, `revision-ahead`; against a restarted session,
  `session-mismatch` — all three, plus `invalid-param` for a bare
  `from_revision`, land on the ack before the stream ever spawns, so the
  connection stays usable for a plain re-subscribe. `tab.effect` (bells,
  clipboard writes) is never replayed, driver included — only live
  clients see it — while `notification.fired` is. No protocol bump: the
  capability rides `session.identify.features` as `events_resume`.
- **`gx.remote` metadata key surfaces a gx session's remote lane (#425)** —
  when gx's remote lane is up it stamps its loopback base URL (`gxRemote`,
  e.g. `http://127.0.0.1:2421`) onto every hook payload; roost now forwards
  a validated token-free loopback URL as `metadata["gx.remote"]` on the
  owning tab, and `roostctl doctor` prints it. It's a discovery hint, not
  liveness — gx never clears its own announcement and roost's metadata has
  no delete channel, so the key can outlive the lane; see the [Agent
  Hooks](docs/guides/agents.md#gx) guide for the naming convention and the
  consumer contract a client still has to follow before using it.
- **Remote host sections drag-reorder too (#398)** — dragging a project row
  or a tab pill inside a connected host's section now reorders it exactly
  like the LOCAL section always could; the drop routes over that host's own
  connection as `project.reorder` / `tab.reorder` rather than mutating the
  local workspace, so the new order lands on — and persists on — the
  session itself, across a disconnect/reconnect or a relaunch. Both reorder
  ops also accept the host-qualified `h<incarnation>.<id>` spelling on the
  UI socket now, the same form `tab.focus` and `tab.dump` already use, so
  the drag isn't the only way to drive it; `app.sidebar_dump` gained an
  additive `hosts` array reporting every *saved* host's own project/tab
  order, each entry carrying that host's connection state — a disconnected
  section lists the rows it retained, a never-connected one lists none.
  A refusal that crosses from the session onto the UI socket with no
  code the UI socket already speaks (a latched `session.stop`, say) now
  reports as the new `host-unavailable`, with the session's own sentence
  kept in the message.
- **Five coding agents get the tab dot, automatically (plan 046)** — Claude
  Code, Codex, grok (and its fork gx), cursor-agent, and OpenCode all now
  drive the running/needs-input/idle/failed indicator, the sidebar rollup,
  and the desktop banner, on local tabs and on host-session tabs alike.
  Wiring happens by itself the first time Roost starts (`agent-hooks = auto`
  in `config.conf`, the default): Roost merges one hook entry per event into
  each present agent's own config file (`~/.claude/settings.json`,
  `~/.codex/hooks.json` + `config.toml`, `$GROK_HOME/hooks/roost.json`,
  `~/.cursor/hooks.json`, an OpenCode plugin) beside whatever else is
  already there, pointed at a new `$ROOST_AGENT_HOOK` environment variable
  every tab gets rather than a baked-in absolute path — so the exact same
  entry works after a relocated install, on a second machine, or over a
  host session, and is inert (drains stdin, answers `{}`) on a machine
  where Roost isn't running — every reference in it is spelled
  `${NAME:-}`, because grok checks a hook's variables itself before it
  spawns a shell and would otherwise draw a red "hook not executed" row per
  tool call in any non-Roost terminal; codex's `SessionEnd`/`Interrupt`
  entries carry its 3 s cap so it stops warning about clamping them on
  every launch. `roostctl agent ensure/install/uninstall/status`
  are the manual controls; `agent-hooks-skip` opts individual agents out;
  `agent-hooks = off` stops future wiring, and `roostctl agent uninstall
  --all` (or a fresh `agent ensure`, which reads the same key) takes
  Roost's entries back out — to the byte where the format allows it
  (TOML), or semantically where it can't (JSON — see the [Agent
  Hooks](docs/guides/agents.md#the-guarantee) guide for the guarantee
  stated in full). A connecting host session gets
  the same treatment via a new `session.set_agent_hooks` op, sent after
  every connect with the client's own config — `off` on a host means
  unwire, since there's no one there to clean up by hand otherwise. Riding
  along: the long-standing defect where approving a Claude permission
  prompt left the dot orange until the whole turn ended is fixed (Claude
  now hears `PreToolUse`/`PostToolUse`, so the dot returns to blue when the
  approved tool finishes); `roostctl doctor` gains a per-agent `Agents`
  section (wired version, ownership, codex's hook-trust hash, a warning if
  the retired `~/.config/roost/claude-settings.json` is still around); and
  the agents palette (`Cmd-Shift-O` / `Alt-Shift-O`) now names which agent
  owns each row instead of just `project · tab`.
- **`roostctl session autostart install|uninstall` brings `roost-session`
  back after a login or reboot (#424)** — a generated `systemd --user`
  unit (`~/.config/systemd/user/roost-session.service`, `Type=simple`,
  `Restart=on-failure`, `KillMode=mixed`) or launchd LaunchAgent
  (`~/Library/LaunchAgents/ai.stridelabs.roost-session.plist`,
  `KeepAlive: {SuccessfulExit: false}`) replaces the doc's old
  copy-this-plist-by-hand recipe. It's opt-in and never interrupts a
  session that's already running: installing over one already up prints
  `installed; a session was already running and was not interrupted` and
  leaves the unit stopped-on-purpose, while `uninstall` says `stopping
  the supervised session` before it does exactly that. Every artifact
  roost writes carries an ownership marker, so a foreign file — including
  one hand-copied from the old doc recipe — reads as foreign and needs
  `--force` once before roost will replace it. `roostctl session status`
  now prints an `autostart=` line in both its running and not-running
  branches (`installed (… → <binary>)`, `not installed`, `unavailable`,
  with `foreign file` / `binary missing` qualifiers), read straight off
  the artifact on disk; it deliberately never asks `systemctl` or
  `launchctl` anything, which stay the authority on enabled/active state.
  See the [Host Sessions
  guide](docs/guides/host-sessions.md#surviving-reboots-launchd) and
  [`cli.md`](docs/reference/cli.md#session-autostart-install-uninstall).
- **The `vt` fallback now says so, and a remote host can fix itself
  in-app (#447)** — a libghostty build skew used to connect silently
  (#420): the dot stayed green, and the only way to notice was to poll
  `roostctl host status --json` after attaching a tab. A reduced-fidelity
  connection now grows a `reduced fidelity` indicator on the sidebar's
  host band the moment the connection is up — before any tab has
  attached — and the first `vt` attach on it prints a one-time
  status-bar sentence naming what is off (links, the alternate screen,
  soft wrapping). On an ssh or `localhost` host that indicator, a row
  underneath it, and a command-palette verb (`host:update:<id>` /
  `host:restart:<id>`) all open the fix directly: over ssh, the
  bootstrap consent card with the fidelity reason on it — no more
  "needs a restart" prompt first, and no more manual-only route, closing
  the one gap #420 left open; on `localhost`, the restart confirmation.
  A Unix-socket target still gets no in-app offer — Roost cannot reach
  in over a bare forwarded socket — and the indicator there is
  informational text only. `roostctl host status` reports the new
  `connect.reduced_fidelity` field (readable before any attach, unlike
  `payload_kind`) and prints the fidelity line in its human form. See
  the [host sessions
  guide](docs/guides/host-sessions.md#when-a-build-skew-connects-instead-the-vt-fallback).
- **A reconnect to a session you already hold a checkpoint for resumes
  instead of re-snapshotting (#442)** — every reconnect, automatic or
  manual, used to subscribe fresh and pull a whole-workspace `tab.list`,
  purging every row keyed on the dead connection until the new one
  landed — the host's sidebar section rendered empty for the whole
  prologue, seconds over a flaky ssh link. A reconnect to the same
  session now calls the resumable `events.subscribe {from_revision,
  session_id}` (#422) from the last revision it applied, taking **no**
  `tab.list`, and the sidebar keeps drawing the carried rows through
  `Connecting` so the section never blanks. Any of the three refusals
  (`replay-expired`, `revision-ahead`, `session-mismatch`) — a gap too
  old, a session that outran the checkpoint, or a restarted session —
  falls back to the ordinary fresh subscribe-and-snapshot on a new dial
  and is never fatal to the connection. `roostctl host status` reports
  `connect.resumed` and `connect.from_revision`, and the new `tabs`
  count, so a caller can see the no-flicker behavior directly instead
  of scraping the sidebar.

### Removed

- **The dead `Workspace::pty_replaced` / `Workspace.ptyReplaced`
  primitive is gone on both platforms (#424)** — a no-op for users. It
  existed to reset agent ownership when a PTY was replaced in place, for
  a hard-restart-keeping-the-tab-id respawn ([#170](https://github.com/charliek/roost/issues/170))
  that was never built; on both platforms today a PTY exit closes the
  tab outright, and the primitive's only caller left was its own unit
  test. The Mac's call site ran immediately before `tab.closed` and
  emitted one now-removed `agent_report.changed` commit that nothing
  observed, since the row is dropped a commit later regardless. #170's
  hard-restart, if it ever ships, rebuilds the same reset inside its own
  respawn commit.

### Changed

- **`tab.reorder` / `project.reorder` narrow the ids they accept** — the
  new host-qualified ref parser requires the canonical integer spelling,
  so non-canonical forms like `"+4"` or `"04"`, which the old codec
  silently accepted, are now refused. Every first-party caller already
  writes the canonical form; this should only bite a caller that relied
  on the old laxness.
- **Claude's post-turn nag no longer banners a session you already saw
  finish (plan 046).** The `idle_prompt` notification Claude fires ~60
  seconds after a turn goes quiet used to raise an info banner
  unconditionally, including for a turn that had already reported
  `finished` (or `failed`, or `waiting`) by other means. It's now guarded
  to only fire from a `working` tab, so it can move a genuinely-still-open
  turn to finished but can no longer re-announce, or overwrite, a state you
  already saw. Cursor's `stop` (fired up to three times per turn) and
  grok/gx's own idle nag are guarded the same way, for the same reason.
- **`roostctl claude install` merges directly into `~/.claude/settings.json`
  instead of writing a separate settings file and a shell alias (plan
  046)** — it's now a bare alias of the new `roostctl agent install claude`.
  It no longer writes `~/.config/roost/claude-settings.json` or prints an
  `alias claude='claude --settings ...'` snippet, and it exits 0 rather
  than 1 when Claude is already wired (there is no `--force` any more). If
  you're on the old alias, `roostctl doctor`'s new
  `agent.claude.legacy_settings` check names the two cleanup steps — see
  the [Agent Hooks](docs/guides/agents.md#legacy-claude-settings)
  guide.

### Fixed

- **grok/gx: a continued turn's `stopHookActive: true` Stop fires keep the
  tab `working`, and a failed turn's banner carries the reason (#423)** —
  gx's `Stop` hook is a gate: when a blocking Stop hook continues the
  turn, `Stop` fires again with `stopHookActive: true`. Each such fire now
  keeps the tab `working` (instead of re-reporting `finished` with another
  "Turn complete" banner) until gx's own `idle_prompt` Notification
  settles it. The very first fire of a turn still reports `finished`
  until the next tool event or gated fire — a passive observer cannot
  tell whether a gate will block it (see the agents guide). Separately,
  `StopFailure` banners now read gx's `errorDetails` (the adapter was
  reading `error_details`, a key gx never emits) instead of always
  falling back to the bare classified error name.
- **`tab.close` no longer answers `not-found` for a tab it just closed
  (#416)** — the op sent the child its hang-up first and removed the
  workspace row second, and the exit path that hang-up starts also
  removes the row; when it got there first the op failed with `tab N not
  found`. The row now leaves the workspace before the PTY is torn down,
  the order `project.delete` always used. `roostctl tab close` was the
  only surface that showed it — both UIs already treated the race as a
  closed tab — and `attach_stream_test::exit_during_attach` was the flake
  it produced on CI.
- **A tab whose child stops reading can no longer park the engine's PTY
  writer (#409)** — on Linux a master `write(2)` blocked on a full slave
  input buffer was never released, not by the child dying nor by the tab
  closing, and it held a runtime worker until the process exited; quitting
  could hang on it. The master is non-blocking now and the writer waits on
  the reactor, ending on the slave's hang-up; the reader waits in `poll(2)`
  for the same reason. macOS was never bitten (that kernel releases the
  write), but both UIs run the one path.
- **A `localhost` host can now start its session with `ROOST_STATE_DIR`
  set (#397)** — a spawned daemon used to inherit the launcher's own state
  dir wholesale and refuse to start against the lock the UI already held.
  It now gets its own dir nested under the launcher's (`<value>/session`),
  so Connect on `localhost` works under the same seam the test harness
  always launches under. `roostctl session start` derives the same way
  under the same seam and now prints the derived path on stderr; a direct
  `roost-session start` under the seam is unaffected and still uses it
  verbatim.
- **`host.status`'s `retry` says why a rung is armed, not just when
  (#399)** — a retrying ssh connection used to overwrite its classified
  failure with the armed-rung line, so `reason` said `reconnecting in 8s
  (3/10)` and never why until the ladder gave up. The retry block now
  carries an additive `reason` alongside the rung's numbers, present
  whenever the drop that armed it was classified.
- **Tab now moves through the Add Host dialog** — previously Tab did
  nothing there; it now cycles Name → Target → Add & Connect → Cancel and
  back, Shift+Tab reverses, and Enter or Space activates whichever button
  the ring is on.

### Internal

- **`HostConnSet` keeps one `HostEntry` per saved host, and three `App`
  guards are now unit-tested (#383, #386)** — no user-visible change.
  `HostConnSet` used to carry six parallel `HashMap`s keyed on a saved
  host's id (`conns`, `retained`, `generations`, `ssh`, `outages`,
  `bootstrap_notes`); it's one `HashMap<String, HostEntry>` now, so a
  host's lifecycle has one clearing path per operation instead of six
  chances to forget one. And the three guards that decide whether a
  reconnect fires, whether a landed tunnel is dialed, and whether a
  saved-host connect proceeds while the app is quitting — previously
  reachable only through `App::bootstrap`, which measures fonts, builds
  a tokio runtime, and binds the IPC socket, so they had e2e coverage or
  none — are lifted onto free functions over `ExitState`, `HostConnSet`,
  and `BootstrapsInFlight` in a new `app/host_lifecycle.rs`. Each now has
  a unit test that fails when the guard is deleted.

## v0.0.19 — 2026-09-04

_The host-sessions release: `roost-session` ships in `Roost-Iced.app` and
the `.deb` (plus standalone `roost-session-<v>-linux-{amd64,arm64}`
assets), a saved SSH host reconnects itself after a drop, a connection's
state is readable on the wire (`host.status`) and from `roostctl host
status`, a daemon that cannot start says so once instead of retrying
forever, and the desktop-notification path is honest about authorization
on macOS._

### Added

- **macOS gets local host sessions too (HS-4b, plan 041)** — `roost-session`
  now ships inside `Roost-Iced.app` itself (`Contents/MacOS/roost-session`,
  individually signed and codesign-verified, with a symlinked copy beside
  the bundled `roostctl` so its own sibling-binary lookup resolves it too),
  so the Mac build offers the same `localhost` surface Linux has had since
  HS-2: a fresh install's palette shows **Connect Host: localhost** (saves
  it and starts the daemon if nothing's listening yet), and a saved
  `localhost` host auto-reconnects — connect-if-present, never spawning on
  its own — the moment Roost launches. Quit Roost-Iced and a `localhost`
  host's projects and tabs keep running in `roost-session`; relaunch and
  they're still there, exactly like any other host — an ordinary local tab
  still ends with the app, unchanged. Reaching a *remote* machine as an SSH
  host is still Linux-only — Roost can't install or start `roost-session`
  on a Mac for you yet — and the Swift `Roost.app` still has none of this
  feature, permanently. For a Mac that should come back up after its own
  reboot, the guide now has a [launchd LaunchAgent
  recipe](docs/guides/host-sessions.md#surviving-reboots-launchd) —
  `KeepAlive: {SuccessfulExit: false}` so a deliberate **Stop Session**
  stays stopped instead of launchd resurrecting it. See the [Host Sessions
  guide](docs/guides/host-sessions.md#macos-note) for the full shape.
- **A saved SSH host reconnects itself after a drop (HS-4 slice 1, plan
  040)** — once a host has actually connected, Roost now treats a
  mid-session drop the way it always treated `localhost`: the sidebar
  dot goes grey and the band shows `reconnecting in Ns (k/10)` while it
  retries on its own, starting at a 1-second delay, doubling with
  jitter, capped at 30 seconds between tries. Ten attempts without
  success and it settles — `reconnect gave up after 10 tries` — with
  ↻ Reconnect, which never left the screen, as the recovery. No new
  setting: the choice is the Connect you already made. Launch is
  unchanged — a saved SSH host still never auto-connects when Roost
  opens, and auto-reconnect still never spawns a session on the far
  side, so a rebooted remote with nothing listening settles on "no
  session" immediately rather than retrying (running `roost-session`
  as a lingering `systemd --user` unit is the fix for a host that
  should survive its own reboots). Not every drop gets a ladder: a
  changed host key is never retried (retrying a possible
  machine-in-the-middle in a loop would be a bug, not a feature), an
  unknown host key or a refused login need a person to do something
  Roost can't do non-interactively, and a session that's genuinely
  gone settles rather than spins. An explicit Disconnect clears any
  scheduled retry, a laptop sleeping through a drop spends no attempts
  while it's closed, and a resume that looks like a long sleep
  restarts the ladder at its base delay instead of burning attempts
  while the radio reassociates. Riding along: six previously-unbounded
  stderr drains in the SSH transport (#379) are now bounded, and a
  drain that couldn't finish reading in time is treated as
  unclassifiable rather than silently downgraded into a retryable
  failure — the fix that keeps the ladder from ever turning a changed
  host key into a security hole. See the [Host Sessions
  guide](docs/guides/host-sessions.md#adding-a-remote-host-over-ssh)
  and
  [`docs/development/host-sessions.md`](docs/development/host-sessions.md#the-leasetakeover-lifecycle)
  for the shape.
- **Roost can install and upgrade `roost-session` on a remote host itself
  (HS-3 bootstrap slice, plan 039)** — the SSH transport (plan 038, below)
  still needed `roost-session` already installed, at the exact right build,
  and started; this closes that gap. On an attended connect that fails
  because it's missing, not running, or on a stale build, Roost probes the
  far side over one job-private `ssh` `ControlMaster` — an eight-rung
  candidate ladder (`~/.local/bin`, the non-interactive `PATH`, `/usr/bin`,
  then the common brew/nix locations) shared by the probe and the connect
  path itself, so anything the probe finds is something a connect can
  already reach — and, only with explicit consent, installs or updates it.
  The consent card names exactly what will happen (install/update/start),
  where, and where the bytes are actually coming from — this Roost's own
  binary, a checksum-verified release download, or an override — before
  anything is touched; Cancel mutates nothing. An install streams to a
  `.tmp.$$` file reserved against symlink/overwrite tricks, verifies the
  staged binary identifies as the client's exact build **before** it ever
  replaces the destination, and backs up any incumbent so a failure after
  that point restores it rather than leaving the host with nothing working.
  An upgrade runs install → stop (lease-free, no takeover) → wait for the
  old process to actually finish exiting → start → verify the new session's
  identity → reconnect — deliberately not "stop then install", so a failed
  install never touches a running session. The install rule requires an
  exact match on `app_version`, `session_protocol`, and `libghostty_build`
  — stricter than the runtime attach gate, which still accepts an
  already-running adjacent-version session unchanged. `roostctl` gains no
  install/upgrade surface of its own, and an IPC-originated connect never
  raises the consent dialog — this is an attended, in-app flow only, never
  something a script or a machine can trigger. The NotFound and NoSession
  troubleshooting rows and the remote "needs restart" dialog now describe
  these in-app offers instead of pointing at a manual `ssh` session. See
  the [Host Sessions guide](docs/guides/host-sessions.md#troubleshooting)
  and [`docs/development/host-sessions.md`](docs/development/host-sessions.md#bootstrap-installupgrade-over-ssh)
  for the shape, including the trust chain's honest limits (checksum
  verification proves the bytes match the release, not the release's own
  integrity — artifact signing is future work).
- **Host sessions reach a remote machine over SSH directly (HS-3 transport
  slice, plan 038)** — a saved host's target can now be an SSH destination
  (`workbox`, `user@host`, `ssh://user@host:port` — only the `ssh://`
  spelling carries an explicit port) as well as a Unix socket path, in
  both the palette's **Add Host…** dialog (its Target field) and
  `roostctl host add --target`. Roost drives its own `ssh` — one `ssh -T
  … exec roost-session client-bridge` per accepted connection (control,
  events, each attached tab), multiplexed over a private, per-host
  `ControlMaster` so a long-lived events stream can never block the next
  tab's exec behind it. The generated `ssh_config` includes the user's
  own `~/.ssh/config` first, so their own keepalives win, falling back to
  a `ServerAliveInterval`/`ConnectTimeout` pair only where the user has
  none; every invocation runs `BatchMode=yes` — key/agent auth only, this
  slice, no password or interactive 2FA prompt. A failed connection is
  classified into one of six families — changed host key (never offers
  to accept it), unknown host key, auth refused, no session running,
  `roost-session` not found (the non-interactive-PATH gotcha), or an
  opaque transport failure — each with copy written for a user to act on,
  surfacing in the sidebar band as `disconnected — <reason>` and, for an
  attended attempt, a toast. `--verify` (`host add` and the dialog's "Add
  & Connect") probes an SSH target with a one-shot, mux-less `ssh` exec
  before saving, so a typo or an unreachable host is caught immediately
  and nothing is left running either way. `ROOST_SSH_BIN` overrides the
  `ssh` binary. See the [Host Sessions guide](docs/guides/host-sessions.md#adding-a-remote-host-over-ssh)
  and [`docs/development/host-sessions.md`](docs/development/host-sessions.md#transport-ssh-hosts)
  for the shape, and [`docs/reference/ipc.md`](docs/reference/ipc.md) for
  the wire (byte-identical over SSH — the far side is a pure byte pump).
  The bootstrap half of this milestone (auto-detect/install/verify a
  remote `roost-session` binary) is not part of this slice — see the
  HS-3 bootstrap entry above, which shipped it separately.
- **`session.set_focus` closes the HS-2 attention gap** — a session now
  learns the client's real focus (window focus + which tab is selected),
  pushed right after `session.connect` and on every edge that moves it,
  so the [focus-suppression rule](docs/guides/notifications.md#focus-policy)
  applies to the tab a client is actually looking at instead of
  whichever tab a headless session's `window_focused = true` default
  happened to pick. The stated focus is deliberately short-lived — a new
  lease, the reporting connection closing, or the lease's last
  connection closing all revert the session to "nobody is looking" — so
  a stale claim can never linger past the client that made it. A session
  one release older answers `unknown-op` and keeps HS-2's behavior. See
  [`docs/reference/ipc.md`](docs/reference/ipc.md#sessionset_focus).
- **The iced UI attaches to host sessions (HS-2, #374)** — closing Roost
  no longer has to kill what is running in it. Save a `roost-session`
  socket as a **host** (`roostctl host add` or the palette's **Add
  Host…** dialog, which validates by dialing `session.identify` before
  saving) and the sidebar grows a per-host section beside your local
  projects — a connection dot (green connected, amber connecting/needs
  a restart, grey disconnected), a right-aligned agent rollup, and the
  same project/tab rows a local workspace renders, agent surfaces and
  all: the agents palette, the notification inbox, and desktop banners
  are host-blind. Attaching is on-focus — switching to a host's tab
  dials that session's data plane, hydrates a client-side terminal
  from its snapshot (never blanking mid-retry), and detaches when you
  switch away, so client memory and connections stay bounded at one
  per host. Disconnect (closing the window, or the palette's
  **Disconnect Host**) leaves the session's shells running, dimmed but
  listed, with an inline "↻ Reconnect"; **Stop Session** actually ends
  them. A second window connecting **takes over** — the displaced
  window keeps its last frame on screen, dimmed, under a banner with a
  **Reconnect here** button. On localhost (Linux only — no packaged
  `roost-session` on macOS yet, so the surface there is Add Host
  pointed at a remote machine's forwarded socket) a saved host
  auto-reconnects on launch if the session is already running, never
  spawning one silently; an explicit Connect spawns it if needed. A
  build or protocol mismatch — the feature's most common failure mode,
  hit on every upgrade — puts the host in `needs-restart` and raises a
  dialog instead of a corrupt screen: **Restart session** composes
  `session.stop` + relaunch + reconnect client-side (every tab reopens
  as a fresh shell in its directory; running programs end), and a
  mismatched *remote* host, which this client cannot restart, gets a
  docs pointer instead of a dead button. Creation follows context:
  `⌘N`/`Alt-N` and the **+ New Project** button create on the selected
  project's host, `⌘⇧N`/`Alt-Shift-N` opens a **New Project on…**
  host picker, and `⌘T`/`Alt-T` never lands a tab on a different host
  than its project. Every palette verb has a `roostctl host` (`add`,
  `list`, `remove`, `connect`, `disconnect`) equivalent driving the
  identical op, so a script can never diverge from a click. Two small
  additive server changes ship alongside: a session now forwards a
  tab's bell and OSC 52 clipboard writes to whichever client holds the
  lease (`tab.effect` events, 256 KiB clipboard cap), and
  `session.set_theme` seeds a session's terminals with the connecting
  client's palette so query replies match what the client actually
  renders. See the [Host Sessions guide](docs/guides/host-sessions.md)
  and [`docs/reference/ipc.md`](docs/reference/ipc.md) for the wire.
- **Host sessions can be attached to: server terminals and a binary
  attach stream (HS-1b, #363)** — every tab a `roost-session` spawns now
  has an authoritative libghostty terminal behind it, fed synchronously by a
  per-tab task over a bounded channel (so a runaway child feels backpressure
  instead of the session growing without bound). Three things follow. A
  program that asks the terminal who it is (DA/DA2), where the cursor is
  (DSR), or what a palette entry holds now **gets an answer with nobody
  attached** — it used to hang or time out. `tab.dump` and
  `tab.dump_resolved` are **served headlessly** from that terminal, through
  the same densifier the iced UI paints with, so the two cannot drift. And a
  client can **attach**: `tab.attach` hands out a single-use ticket, a second
  connection presents it, and the session streams the tab's whole terminal as
  a snapshot followed by live output — length-prefixed binary frames, 1 MiB
  cap, `EXIT` always last. A client that drops and comes back can **resume**
  from where it left off out of a per-tab replay ring, scoped by a random
  per-process epoch so a restarted session can never silently hand back a
  stale stream. `session.connect` mints an **interactive lease** — the
  authority to drive a session, as opposed to read it — and `takeover: true`
  closes every connection the previous holder had, telling each one why:
  events connections get a terminal `session.stopping` envelope, data
  connections an `ERROR` frame. `session.stop` labels its teardown the same
  way instead of closing bare, closing the last of HS-1a's three documented
  deviations. **Breaking change:** `events.subscribe` on a session socket now
  requires that lease and answers `connect-required` without one;
  `session_protocol` is `2`. Nothing about a UI socket changes — it answers
  `unknown-op` for the new ops and never sees a binary frame. See
  [`docs/reference/ipc.md`](docs/reference/ipc.md#data-plane) for the wire.
- **`roost-session`, an opt-in headless host-session daemon (HS-1a, #363)** —
  a new `crates/roost-session` binary that owns a workspace + PTY supervisor
  with no UI attached, for a session left running with nobody watching (e.g.
  a remote host). Start/stop/inspect it with `roostctl session
  start|stop|status`; the Linux `.deb` now ships `/usr/bin/roost-session`
  alongside `roostctl`. It daemonizes with a readiness handshake (`start`
  only exits 0 once a session actually answers), installs `umask 0077` so
  everything it creates lands owner-only, enforces a same-UID peer check on
  its socket (no UI socket does), and hydrates its saved tab layout headlessly
  (fresh shells, per-tab OSC drains — titles/cwd/notifications keep working
  with no terminal or renderer present). `SIGTERM`/`SIGINT` converge on the
  same graceful-shutdown path as `session.stop` (self-dialing the socket;
  a broken socket falls back to flush-and-exit without the reap report): every in-flight mutating op
  is let finish, the layout is flushed, then every PTY is hung up (escalating
  to `SIGKILL`), and the reply carries a reap report
  (`{reaped, killed, abandoned}`). The wire adds `session.identify`,
  `session.stop`, and `events.subscribe` on this socket only — UI sockets
  answer `unknown-op` / `not-implemented` for these, unchanged.
  (`events.subscribe` shipped leaseless here and is lease-gated by the HS-1b
  entry above — both land in this release, so there is no leaseless form to
  write against.) A new `roostctl
  --socket` reaches a running session for any other op; a dedicated
  `make e2e-session` pytest lane (`tools/roosttest/test_session.py`) is now a
  required CI gate. See [`docs/reference/ipc.md`](docs/reference/ipc.md#session-sockets)
  for the full contract.
- **Terminal snapshot encode + decode in `roost-vt`** — `Terminal::snapshot()`
  serializes a live terminal to libghostty's snapshot byte stream, and
  `SnapshotDecoder` restores one: buffered in a single call, or progressively,
  handing back a renderable, typeable terminal as soon as the stream's READY
  marker arrives and prepending scrollback pages as more bytes land. The
  wrapper owns its buffering and enforces its own size caps, so incomplete
  or over-limit input is refused before it reaches libghostty (corrupt
  payloads remain libghostty's per-record CRC check to reject). Groundwork
  for host sessions (#363); nothing in either UI consumes it yet.
- **`host.status` reports every saved host's connection state (#382)** —
  a new read op returns, for every saved host, its live state, retry
  schedule, and the sidebar band's own rollup text, so `roostctl host
  status [--id] [--json]` can answer "what is this host doing right now"
  without rendering the window. `roostctl host list` also grows a
  best-effort `state=` column, backed by the same op.
- **A missing-daemon E2E lane (#392)** — `make e2e-host-missing-daemon`
  (and its CI variant) proves that a `localhost` host with no reachable
  `roost-session` binary settles once, with the right reason, instead of
  spinning forever.
- **`tools/session/dev-session.sh` builds a matching `roost-session` for
  a remote (#393)** — one script builds `roost-session` on a target's
  own architecture in a shed, fetches the artifact, cross-checks it
  against the remote's `uname -m`, and launches Roost against it via
  `ROOST_SESSION_INSTALL_BIN`, closing the loop from "I have a dev build
  on this Mac" to "a Linux remote runs the matching daemon."
- **A live SSH-reconnect harness ships in-repo (`tools/session/live/`,
  #384)** — `mutate.sh selftest` and `live.sh`'s lanes exercise
  auto-reconnect against a real `sshd`, asserting on `roostctl host
  status` instead of scraping log lines. Not a CI lane — see the
  `linux-test` skill for how to run it from `roost-dev`.

### Removed

- **The GTK UI is gone** — `crates/roost-linux` (gtk4-rs + libadwaita) has been
  removed from the repo. Linux ships the iced UI, which v0.0.18 already made
  `/usr/bin/roost`; nothing about an installed package changes. The hidden
  alias desktop entry that keeps pre-rename launcher pins working stays.

### Changed

- **A saved host's `target` string is classified differently (host-sessions
  HS-3, plan 038)** — now that a target can mean an SSH destination as well
  as a socket path, the rules changed to keep the two unambiguous: the
  string is **trimmed** before it's read; a target with no `/` in it (a
  bare word like `roost.sock`, or a relative filename) used to resolve as
  a same-directory socket path and now reads as an SSH hostname instead —
  spell a relative socket path `./roost.sock` to keep the old meaning;
  and an **empty** target (after trimming) is now refused outright rather
  than saved as a host nothing could ever reach. Absolute paths, `~`-paths,
  and explicit `./`/`../`-paths are unaffected.
- **`roostctl --target linux` replaces `--target gtk`**, and
  `ROOST_BUNDLE_PROFILE=linux` replaces `=gtk`. There is no `gtk` alias: a
  stale `ROOST_BUNDLE_PROFILE=gtk` makes `roostctl` fail loudly with
  ``expected `mac`, `linux`, or `iced` ``, and a UI started with it logs a
  warning and falls back to its default profile. Tab environments are
  unaffected — the UI injects `ROOST_SOCKET`. **On Linux, all on-disk paths
  are unchanged**: an installed package's socket, `state.json`, both locks,
  and log directory resolve exactly as before. (The macOS *dev-mode* paths
  for this profile — never shipped — move from `Roost-gtk` to
  `Roost-linux`.)
- **`identify` and `roostctl doctor` report `app_label: Roost-linux`** for the
  packaged Linux profile, where they used to report `Roost-gtk`. The label is
  cosmetic — no path or app id is derived from it on Linux.
- **libghostty-vt bumped to ghostty main tip (#333)** — pin `c74f6d56`
  (2026-04-25) → `f2d5758f6` (2026-08-26), zig 0.15.2 → 0.16.0. Scrollback now
  genuinely honors the configured line limit: the old pin treated the
  configured value as a byte cap internally, silently capping history at a few
  hundred rows regardless of the configured count (a 2000-line setting never
  actually delivered 2000 rows); it does now. Three
  new libghostty error codes (`IO_ERROR`, `LIMIT_EXCEEDED`, `REJECTED`) are
  mapped into `roost-vt`'s `Error` enum.

### Fixed

- **macOS notification authorization is re-read on focus gain, on both
  UIs (#355)** — granting or revoking notification permission in System
  Settings used to need a relaunch to take effect; the authorization
  state was read once at launch and cached for the life of the process.
  Both the Swift app and Roost-Iced now re-read it whenever the window
  regains focus — the moment a user comes back from System Settings —
  so the next notification honors the change with no relaunch.
  `app.notification_status`, the test-gated IPC op for reading this
  state, is now served by both UIs with the same
  `{backend, reason, authorized}` shape.
- **Focusing a tab acknowledges its notification, in both cores
  (#369)** — `tab.focus` moved the selection but left the tab's
  pending-attention badge standing, so a hook- or `roostctl`-driven
  focus (not just a click) left the sidebar lying. The core now clears
  the notification and emits `tab.notification` with
  `has_pending: false` right after `active.changed`, in the same
  batch, which also retires the tab's inbox row and its project's
  rollup — all three derive from the same bit. Focusing an
  already-active badged tab acknowledges it too.
- **`tab.open` with no `cwd` resolves correctly on iced (#266)** — a
  bare `roostctl tab open` landed the new shell in the *UI process's*
  own working directory instead of the project's; Mac already resolved
  this, and the shared Rust engine's IPC handler now does too. The fix
  lives in `Workspace::open_tab`, the one path every caller reaches, so
  title derivation and agent git metrics — both keyed off tab cwd —
  stop drifting from where the shell actually runs.
- **The mac release job now proves the Sparkle keypair matches before
  building (#356)** — it previously checked only that the public key
  wasn't the known throwaway string. It now derives the release
  secret's public half (through both base64 layers, comparing the
  trailing 32 bytes directly for a 96-byte legacy key) and checks it
  against the committed `SUPublicEDKey` in the Info.plist template —
  the same real derive-and-compare the iced job has done since #349,
  with no prerelease exemption. A mismatched pair used to ship a DMG
  that could never self-update, with nothing else noticing.
- **`bundle.sh`'s nested signatures can no longer fail silently under
  `ROOST_ALLOW_UNSIGNED=1` (#391)** — mirroring the guard
  `bundle-iced.sh` already had, a tolerated failure signing the nested
  `roostctl` or Sparkle framework now blocks the outer `Roost.app`
  signature instead of producing a bundle that looks signed but isn't,
  and a permanent post-bundle verification step catches a broken or
  partial seal the pre-sign guard alone could miss.
- **A `localhost` session that can't start settles once, with the
  reason (#392)** — with the `roost-session` binary unreachable, the
  band used to redial forever, re-burying the spawn ladder's actionable
  message under a bare `io error: No such file` on every retry. It now
  settles once instead, and the reason travels with it: `detail` on the
  wire and under the row in `roostctl host status` names which rung
  failed.
- **A wrong-architecture `ROOST_SESSION_INSTALL_BIN` is refused before
  it streams (#393)** — pointing the install seam at a binary built for
  the other architecture used to fail late, at the remote's staged
  verify, with nothing more than the remote shell's `Exec format error`.
  It's now refused up front, naming both architectures.

## v0.0.18 — 2026-08-25

**Linux switches to the iced UI, and macOS gains an experimental iced build
alongside the Swift app.** The `.deb` now ships the Rust + iced UI as
`/usr/bin/roost` in place of the GTK app — same socket, same `state.json`,
same log directory, so `roostctl` and the Claude hooks keep working and
existing installs upgrade with no migration step. On macOS the Swift app
remains the product; a second, opt-in `Roost-Iced.dmg` ships beside it with
its own bundle id, its own Sparkle feed, and its own update key, so the two
can be installed and run side by side.

### Features

- **The Linux package is the iced UI (#314, #315)** — `/usr/bin/roost` is now
  the iced binary, built with the `linux-package` feature so it resolves the
  production `roost` profile the GTK package already owned. GTK4 and libadwaita
  are no longer runtime dependencies. The GTK UI stays in the repo, built and
  tested in CI, as the development/parity implementation.
- **Roost-Iced for macOS, experimental and opt-in (#345, #347, #349)** — a
  Developer-ID signed, notarized `Roost-Iced-<version>.dmg` with a native menu
  bar, Dock badge, Sparkle updater, and desktop notifications. It carries a
  separate appcast (`appcast-iced.xml`) and a separate signing key, so the two
  macOS apps can never offer each other's updates.
- **macOS notifications for the iced build (#349)** — banners route through
  `UNUserNotificationCenter`; clicking one focuses the tab and reveals the
  sidebar. One live banner per tab, replaced in place rather than stacked,
  matching the Linux behavior (the Swift app stacks).
- **Linux notification click-to-focus (#352)** — the iced UI speaks
  `org.freedesktop.Notifications` directly, so the click that activates a
  banner is received on the same connection that sent it. Banners no longer
  expire before they can be clicked, and are withdrawn on click or tab close.
- **The shipped Linux identity is `ai.stridelabs.Roost` (#337)** — window class,
  desktop entry, and app id all match the Mac app. A hidden alias desktop entry
  keeps launcher pins created before the rename working.
- **Terminal parity work across the iced UI** — IME/dead-key input (#313),
  sprite and box-drawing rendering (#310), selection and copy semantics (#330),
  sidebar resize (#294), project lifecycle (#290), tab-strip behavior (#291),
  and a kitty-keyboard/cursor-shape pass (#348).
- **`app.notification_status` (#349)** — a test-mode op reporting the macOS
  notification backend's availability and authorization, so the gated paths are
  assertable end to end.

### Fixes

- **Crash robustness (#308)** — panics in either Rust UI now produce a crash
  report instead of a silent exit, and a malformed font can no longer abort the
  process (#298).
- **Rendering keeps up under load (#296, #306, #307)** — the engine tracks dirty
  rows and pushes frames rather than polling, cutting redundant redraws in both
  Rust UIs.
- **Single-instance locking and PTY exit handling (#331, #332)** — a second
  launch surfaces the running window instead of racing it, and a tab whose
  process exits is reaped deterministically.
- **Wayland dependency closure (#327)** — the `.deb` declares what a Wayland
  session actually needs, so a clean-machine install launches.

### Release process

- **The release publishes four assets** — two `.deb`s, `Roost-<version>.dmg`,
  and `Roost-Iced-<version>.dmg` — and stays a draft until all four are present
  and correctly sized (#329, #349). A missing or truncated artifact leaves the
  release invisible rather than half-published.
- **Two Sparkle feeds** — `docs/appcast.xml` for the Swift app and
  `docs/appcast-iced.xml` for the iced build, each signed with its own key and
  published in sequence so the two bot pushes cannot race.
- **The packaged binary is smoke-tested before publication (#317)** — the real
  `.deb` is installed and launched, and its dependency closure verified in a
  clean container.

### Documentation

- **Docs site migrated from Material for MkDocs to
  [Zensical](https://zensical.org)**, the successor from the same team. Material
  entered maintenance mode in November 2025 and warns on every build that MkDocs
  2.0 will remove the plugin and theming systems with no migration path.
  `mkdocs.yml` is replaced by a native `zensical.toml`; content is unchanged
  aside from a handful of links that `--strict` now validates. The look comes
  from the shared
  [stridelabs-docs-theme](https://github.com/charliek/stridelabs-docs-theme)
  package rather than per-repo config, and fonts are self-hosted, so the site no
  longer requests anything from Google Fonts. Verified against the pre-migration
  build: identical 27-page set and all 374 heading anchors preserved.

## v0.0.17 — 2026-07-31

Agents move out of the palette and into the sidebar. Every running agent now
appears as a row under its project — lifecycle dot, name, elapsed time — with
the active one highlighted, so "what is running, and how long has it been
waiting?" is answerable at rest instead of by opening a palette.

### Features

- **Agents in the project sidebar, both UIs (#270)** — one indented row per
  agent-owned tab under its project: a lifecycle-coloured dot, the agent's name,
  and a right-aligned elapsed time, with the full status as tooltip. Rows reuse
  the agent palette's own filter, name fallback, status vocabulary and ordering,
  so the two surfaces cannot disagree about what is most urgent. Clicking a row
  jumps to that tab and switches the project with it.
- **The active agent and the selected project are visible at once (#270)** — the
  row whose tab is active carries its own subtle highlight, derived from the
  active tab rather than from widget selection, because neither `NSOutlineView`
  nor `GtkListBox` can express two selections. It tracks focus however it moves:
  tab click, palette jump, notification, or `tab.focus` over IPC.
- **`show-sidebar-agents` toggle (#270)** — visible by default; flip it with
  ⌘⇧A / Alt⇧A, the "Toggle Sidebar Agents" palette row, the Mac View menu, or the
  config key. Toggling writes back to `config.conf`, so it survives a restart.
- **`app.sidebar_dump` (#270)** — a read-only, ungated op reporting the sidebar's
  *last-rendered* rows on both UIs, so a refresh the UI failed to run surfaces as
  a failing assertion instead of staying invisible. Documented in
  [`ipc.md`](docs/reference/ipc.md).

### Fixes

- **The agent's own title marker is stripped from row names (#270)** — Claude
  Code prefixes its window title with `✳ `, so a renamed session rendered with
  two status glyphs, its own and ours. Leading marker glyphs are now dropped
  structurally, so a second adapter's marker needs no code change; ASCII titles
  (paths, `[wip] name`) are untouched.
- **The agent dot aligns with the project label (#270)** on both UIs, and the row
  highlight with the project's selection pill, so agents read as a nested list
  rather than an inset block.
- **GTK: the rollup stripe is scoped to the project row (#270)** — it ran the
  full height of the row, which since this release also contains the agent rows,
  double-counting what the per-agent dots already say. It now matches Mac, and
  survives selection (the rules clearing the row background used the `background`
  shorthand, which also resets the stripe's `background-image`).
- **GTK: a cancelled press on an agent row no longer hijacks the next click
  (#270)** — a press nudged past the click threshold but short of the drag
  threshold left a stale jump target that redirected the project's next
  activation, including one arriving by keyboard.
- **Both UIs: `app.sidebar_dump` reports every project (#270)**, including one
  whose creation event has not yet reached the UI, rather than dropping it.
- **Mac: the sidebar flattens for the duration of a project drag (#270)** —
  without it, AppKit proposes child-relative drops once a project is expanded and
  `validateDrop` rejects them, turning most of that project's rows into a dead
  drop zone. GTK flattens for the same reason.
- **Mac: the agent row's activation control carries an accessibility label
  (#270)** — it is transparent and titleless, so VoiceOver announced an
  unlabelled button with no way to tell which agent it focused.

### Tooling & tests

- **Sidebar↔palette parity is now pinned (#270)** — the two surfaces share pure
  builders, but nothing asserted they agree; a dual-target e2e test now checks
  membership, per-project ordering, and per-row content.
- **Dot-layout pixel guards (#270)** — the four lifecycle colours and the dots'
  shared left edge, asserted from real screenshots on both targets. Deliberately
  narrow to stay non-flaky: only solid fills we own, never golden images, text
  metrics, or the Mac selection pill (which follows the system accent colour).
- **The e2e harness no longer writes to a tracked fixture (#270)** — the seed
  config is copied into each session's throwaway state dir, so a test that
  toggles a config key cannot mutate the repo or race the other target.

## v0.0.16 — 2026-07-30

The agent release. The overloaded two-field agent model is replaced by four
independent axes with a derived display state, Claude hook handling moves into a
pure adapter crate, `roostctl doctor` makes the whole integration legible, and
the first agent-UX surface ships on both UIs: an agent palette (⌘⇧O / Alt⇧O)
listing every agent-owned tab with live status and git metrics.

### Features

- **Agent state model — four axes instead of one field (#259)** — shell state
  (OSC 133), agent lifecycle, attention, and ownership are now independent, and
  the displayed state is *derived*, never written. `tab.state` stays a closed
  four-value enum on the wire (`failed` projects onto `needs_input`), and
  `hook_active` is derived from ownership. Documented in `AGENT_ROADMAP.md`.
- **`roost-agent`, a pure Claude hook adapter (#259)** — hook JSON in,
  `tab.agent_report` ops out; no I/O, no clap, no socket. `roostctl claude-hook`
  routes through it, so a second agent is a new module plus an `install`
  subcommand — zero Swift, zero GTK. One canonical hook-event vocabulary
  (`CLAUDE_HOOK_EVENTS` + `canonical_hook_event()`) replaces three copies with
  drifting alias policy.
- **`roostctl doctor` — read-only agent-integration diagnostics (#260)** — 26
  checks across five sections, each scoped (process / ui / tab) so a process
  fact never judges a tab fact. Names *which axis is empty* rather than leaving
  you staring at an absent dot; distinguishes missing / not-a-socket / stale
  sockets, and detects a pre-#259 server exactly via raw-key presence. Never
  repairs, installs, or mutates.
- **Doctor's two-axis status model and summary view (#263)** — checks carry
  `ok`/`warn`/`fail`/`skipped`; observations carry no status. Default output is
  one line per section, `-v` gives the full report, with color.
- **Agent palette — ⌘⇧O / Alt⇧O switcher (#265, roadmap D3)** — lists every
  agent-owned tab with a status dot, project, agent name, status text, elapsed
  age, and async git metrics; up/down/enter to jump, escape to close, rows
  refresh live while open. Pure UI work on the plan-002 state model — no new IPC
  op; `PaletteItemView` gains an optional, additive `agent` payload. Shipped at
  parity on GTK and Mac, with git metrics gathered via short-timeout
  `--no-optional-locks` git subprocesses behind a session-scoped cache.
- **Per-segment colors on the palette's git-metrics column (#269)** — the
  ahead/behind/insertions/deletions segments are colored individually on both
  UIs instead of rendering as one dim run.

### Fixes

- **`idle_prompt` no longer flips a finished session to "Waiting for input"
  (#269)** — Claude fires an idle nag ~60s after a turn ends; it was classified
  as blocking, so a finished session went gray and then orange, indistinguishable
  from one genuinely stuck on a permission prompt. The blocking set is now
  `permission_prompt | agent_needs_input | elicitation_dialog`. Adapter-only, so
  both UIs got it for free.
- **Mac: tab and project positions are allocated from `max + 1`, not `count`
  (#262, #263)** — closing a tab from the middle made the next one reuse a live
  position, and closing from the *front* could render a brand-new tab to the
  left of one that predates it. That's the real cause of "the third tab asked for
  input but the second one turned orange" — the agent model was correct; the row
  it sat next to wasn't. Rust had this fix since #86; Swift never got it. Loading
  a `state.json` with colliding positions now repairs them on both UIs, and four
  position-only sorts gained an `id` tiebreak (they sorted unordered dictionary
  values).
- **The project rollup no longer hides trusted agent state (#259)** — a project
  whose only blocked tab was an agent showed no stripe.
- **Raw OSC 9/99/777 suppression during an agent session (#259)** — documented
  but never implemented.
- **One notification focus predicate (#259)** — banner, unread badge, and inbox
  row disagreed with each other and with the docs.
- **`roostctl`: payloadless claude-hook invocations keep working (#259)**, and
  format characters in doctor output are escaped, closing a padding-aim vector
  (#263).
- **Footer hint bar removed from the palettes (#269)**, mechanism and all.

### Tooling & tests

- **Dual-target e2e coverage for the agent stack** — full lifecycle through the
  real `roostctl claude-hook` binary (#259), `roostctl doctor` end to end (#260),
  tab ordering (#263), and 15 agent-palette cases driving production IPC —
  exact status strings, rank order, effective-vs-raw lifecycle via OSC 133 marks,
  subagent immunity, live refresh, and git metrics against a throwaway repo
  (#265).
- **Palette e2e seeds settle on explicit ready shell states** rather than
  "not unknown", via a fed OSC 133 mark under test mode (#265, #269).
- **`make check` lints at CI parity (#259).**
- **Release assets are labeled by target, not filename (#257)** — "Linux — ARM
  64-bit (.deb)" instead of `roost_0.0.15_arm64.deb`; the download still lands
  under the real name, so apt is unaffected.

## v0.0.15 — 2026-07-26

Kitty-mode keyboard input now carries shifted text correctly on both UIs — you
can type capitals and `?` into Kitty-aware TUIs again — plus a round of terminal
conformance work (DEC 2031, device-query replies, DECTCEM).

### Features

- **Device queries answered via `write_pty` on both UIs (#247, #248)** — the
  terminal replies to DA/DSR-style queries through libghostty's reply buffer
  (`set_write_pty_buffer` in `roost-vt`), so programs that probe the terminal
  get an answer instead of a timeout.
- **DEC 2031: apps are notified on runtime theme switch (#248)** — flipping the
  system/app theme while a TUI is running tells it to re-read its colors.

### Fixes

- **Kitty keyboard mode preserves shifted text (Mac + GTK, #254)** — the
  terminals now tell libghostty which modifiers were consumed to produce
  printable text, so Strix and other Kitty-aware TUIs receive capitals and
  shifted punctuation normally while Shift+Enter stays distinguishable.
  Previously Shift+G reached the app as `CSI 103;2u` ("g" + Shift) rather than
  the text "G", and no capital or shifted punctuation could be typed.
  On Mac this also covers Option-produced text (Option+2 → "™"), matching
  Ghostty's default `macos-option-as-alt = false`: under Kitty mode Option+key
  now sends the composed character rather than an Alt chord, as Roost already
  did outside Kitty mode. Linux is unchanged there — GDK reports the real
  per-layout consumed modifiers, and Alt isn't one of them.
- **GTK reports the base-layout key in CSI-u sequences (#254)** — Kitty CSI-u
  and fixterms identify a key by its *unshifted* codepoint, which GTK derived
  from the shifted keyval. Ctrl+? encoded as `CSI 63;6u` where Ghostty and the
  Mac UI send `CSI 47;6u`, and Ctrl+! kept a Shift that fixterms drops. Now
  looked up from the keymap, closing a Mac/Linux parity gap.
- **Hidden cursor is never drawn — DECTCEM is respected (#246, #248).**
- **GTK: left-edge drag-selection no longer stolen by the paned resize handle
  (#251).**

### Tooling & tests

- **`roost-linux`'s own tests now run in CI (#254)** — `rust-test` excludes the
  crate (no GTK toolchain), so its ~380 unit tests compiled but executed
  nowhere; `gtk-build` runs them now. That immediately caught a PTY smoke test
  whose capture stopped at the first 4 KB chunk, because `Exit` and `Bytes` are
  independent producers on one broadcast channel and `Exit` can arrive first.
  The same assumption in the UI is tracked as #255.
- Rust toolchain 1.85.0 → 1.97.1 (#253).

## v0.0.14 — 2026-06-30

Drag files onto the terminal to attach them, a round of GTK chrome work bringing
it to Mac parity, and the macOS app is now Developer ID signed + notarized.

### Features

- **Drag a file onto the terminal to insert its path (Mac + GTK, #245)** —
  dragging a file (e.g. a screenshot) from Finder / the file manager onto a
  terminal tab inserts its shell-escaped path through the same bracketed-paste
  path as ⌘V, so Claude Code / Codex resolve it as an image and attach it
  (`[Image #N]`). Mirrors Ghostty's drop behavior; the Mac (Swift) and GTK
  implementations share byte-identical escaping. Clipboard paste is unchanged.
- **GTK chrome brought to Mac parity (#231, #236, #244)** — chrome colors, a
  minimal `AdwHeaderBar` title, custom Mac-style tab pills, the tab strip beside
  the sidebar, and the active tab kept scrolled into view.
- **macOS sidebar: flush-left, gapped per-project running rail (#235).**

### Fixes

- **GTK palette focus-walk crash (#234)** — terminal focus grabs now route
  through a rooted-widget guard (clippy-enforced), with focus-ownership
  invariants and a rate-limited glib log writer to bound the warning-storm blast
  radius. Fixes the palette focus crash and the log storms around it.
- **macOS app is now Developer ID signed + notarized (#240)** — gated on the
  full secret set; ad-hoc signing remains the fallback when secrets are absent.
- **release: retry `hdiutil create` on transient "Resource busy" (#243).**

### Tooling & tests

- Wayland pointer-drag guard + CI hardening following #236 (#237); a `popos-test`
  skill for native Pop!_OS COSMIC testing plus shed-based Linux-testing-on-a-Mac
  tooling; and an output-only readiness sentinel that fixes the dominant
  `e2e-mac` flake (`test_env_injected`). Drag-drop's pure-value Mac tests use
  XCTest to dodge a swift-testing-runner SIGTRAP under Xcode 26.x.

## v0.0.13 — 2026-06-26

A microphone/camera permission fix for the macOS app, plus a round of GTK
tab-focus and selection-sync fixes.

### Features

- **Microphone, camera & AppleScript access for programs run in a tab (macOS,
  #232)** — Roost.app now declares the capture entitlements (`device.audio-input`,
  `device.camera`, `automation.apple-events`) plus the paired Info.plist usage
  strings, so a program launched in a Roost tab (Claude Code's `/voice`, audio
  recorders, `osascript`) gets a proper *"Roost would like to access the
  microphone"* prompt instead of failing silently. macOS attributes the request
  to Roost as the TCC responsible app; granting once covers programs in that
  session. The broad personal-information entitlements (contacts/calendars/
  photos/location) are deliberately excluded, and the bundled `roostctl` helper
  is signed narrowly so it never inherits the capture grants. Note: under the
  current ad-hoc signing the grant resets on each app update until Developer ID
  signing lands (#83).

### Fixes

- **GTK terminal focus now matches the Mac UI (#225)** — clicking a tab or
  switching projects reliably refocuses the terminal, and focus is re-asserted
  when the window is re-activated.
- **GTK tab/project clicks sync the workspace core (#228, #229)** — selecting a
  tab via the AdwTabView (and renaming a background tab) now updates the core's
  active selection, so `identify`, restore, and notification routing reflect the
  tab you actually selected.
- **Fixed two GTK crashes** — a segfault when right-clicking an AdwTabView tab
  (context menu), and a crash when tearing down the command-palette overlay.
- **Dropped GTK `Alt+digit` AdwTabView shortcuts** that collided with
  `SwitchProject`.

### Tests + CI

- **e2e-gtk** now runs the real-click terminal-focus regression (non-gating).

## v0.0.12 — 2026-06-15

### Added

- **Five new dark themes** — `Monokai Pro`, `Monokai Remastered`, `Synthwave`,
  `Material Ocean`, and `Oxocarbon`. The bundled set is now 26 themes, all dark.
- **Configurable link-open modifier (GTK)** — hold a modifier and click an
  OSC 8 hyperlink or `https://…` text to open it in your browser. Defaults to
  Cmd on macOS and Alt on Linux; override with `link-modifier = ctrl|alt|super`
  in `config.conf`. Linux users who prefer the conventional Ctrl+click set
  `link-modifier = ctrl`.

### Changed

- **Linux default keybindings are now Alt-centric** — on Linux, `Alt` is the
  single app modifier (the role Cmd plays on macOS), leaving `Ctrl` to the
  shell. Moved: `new_tab` `Ctrl+T` → `Alt+T`, `close_tab` `Ctrl+W` → `Alt+W`,
  tab cycle `Ctrl+Shift+[`/`]` → `Alt+Shift+[`/`]`, `jump_to_unread`
  `Ctrl+Shift+U` → `Alt+Shift+U`, and font zoom `Ctrl+±`/`Ctrl+0` →
  `Alt+±`/`Alt+0`. Unchanged: `Ctrl+1‑9` (switch tab) and the
  `Ctrl+Shift+C`/`V` copy/paste alternates stay on `Ctrl`; all project,
  palette, and clipboard actions were already on `Alt`. Every binding is
  overridable — restore a prior chord via config, e.g.
  `keybind = ctrl+t = new_tab`.
- **Bundled themes are now dark-only** — removed the two light themes (`Atom One
  Light`, `Ayu Light`) and the near-duplicate `TokyoNight Night` (identical to
  `TokyoNight` as rendered by Roost).
- **GTK projects sidebar** refined to match the Mac UI (#220).

### Fixed

- **Clickable links from Claude Code and other tools (#224)** — Roost now
  advertises OSC 8 hyperlink support (`FORCE_HYPERLINK=1`) to programs running
  in the terminal on both the Mac and GTK UIs, so Claude Code's PR-footer links
  (and any `supports-hyperlinks`-gated output) render as clickable links.

## v0.0.11 — 2026-06-07

Theme expansion and a big internal cleanup. The bundled theme set doubles to 24,
the GTK command palette keeps its highlighted row in view, the macOS projects
sidebar gets a readable selection color, and the legacy Go + GTK4 implementation
is retired.

### Features

- **17 more bundled themes — 24 total (#218)** — adds `0x96f`, `Atom`,
  `Atom One Light`, `Ayu Light`, `Ayu Mirage`, `Nord`, `Rose Pine`,
  `Solarized Dark Patched`, `Catppuccin Frappe`/`Macchiato`,
  `TokyoNight Storm`/`Night`, `Gruvbox Dark`, `One Half Dark`,
  `GitHub Dark Default`, `Everforest Dark Hard`, and `Kanagawa Wave`
  (byte-identical to Ghostty's set) across both UIs. Pick one with
  `theme = NAME`; browse them in the command palette's **Select Theme…**.
- **Readable colored selection in the macOS projects sidebar (#216)** — the
  selected project row uses a legible accent fill instead of the washed-out
  default.

### Fixes

- **GTK palette keeps the highlighted row in view (#218)** — arrowing past the
  last visible row, and opening the **Select Theme…** / **Select Font…** pickers
  pre-positioned on the active theme/font, now scroll the highlight into view
  instead of leaving it clipped or off-screen.

### Internal

- **GODELETE — removed the legacy Go + GTK4 implementation (#218)** — now that
  the Swift (macOS) and Rust + gtk4-rs (Linux) UIs are at parity, the original Go
  prototype (`cmd/`, `internal/`, `build/`, `go.mod`, the Go CI) is gone. Its
  working snapshot and full migration history are archived separately. Bundled
  theme files moved into the Rust crate
  (`crates/roost-linux/src/resources/themes/`), kept byte-identical to the macOS
  bundle copy by a `themes-parity` CI job.

### Tests + CI

- **Hardened `test_env_injected` against a shell-startup race (#217).**
- **Bumped GitHub Actions to current major tags (#218)** — `setup-uv@v7`,
  `upload-artifact@v7`, `upload-pages-artifact@v5`.
- **Portable `libghostty-vt` + GTK E2E diagnostics (#219)** — build the
  vendored `libghostty-vt` for a baseline CPU (`-Dcpu=baseline`) so the
  shipped binaries run on any CPU of the architecture; a native-CPU build
  cached across CI's mixed runner fleet was SIGILL-crashing both the GTK UI
  and the `roost-vt` ffi test. Also capture + upload the GTK UI's log so a
  boot failure under xvfb isn't blind (the gap that hid this).

### Docs

- **Extending Roost guide (#214, #215)** — added an advanced multi-step wizard
  (Python) provider example and made the provider example's `PATH` handling
  cross-platform.

## v0.0.10 — 2026-06-05

Provider polish release. Two refinements to the v0.0.9 script-backed provider system
that make providers **portable** and **forgiving**: Roost now hands a provider the path
to its own `roostctl`, and an `activate` that just *does something* no longer has to
sanitize its stdout.

### Features

- **`ROOST_ROOSTCTL` for providers (#212)** — Roost injects `ROOST_ROOSTCTL` (the absolute
  path to its own `roostctl`) into provider scripts, so they can drive Roost without
  `roostctl` on `PATH`. Closes the macOS gap where the `.dmg` bundles `roostctl` inside the
  app (off `PATH`) and a Finder-launched app gets a minimal `PATH`; the Linux `.deb` already
  installs it on `PATH`. Use `"${ROOST_ROOSTCTL:-roostctl}"`. Best-effort — when Roost can't
  resolve its own CLI the var is omitted (and any inherited one stripped) so the `PATH`
  fallback fires.

### Fixes

- **Lenient `activate` stdout (#213)** — an `activate` phase is a side effect, so its stdout
  is now ignored unless it *looks* like a provider payload (a JSON object/array = a drill-down
  sub-menu). A command's incidental output — e.g. the new tab id `roostctl tab open` prints —
  no longer fails parsing with "Provider failed". JSON-shaped output that doesn't parse is
  still reported (so a malformed sub-menu isn't swallowed), and the parse error now names the
  expected shape. The `list` phase stays strict.

### Docs

- **[Extending Roost](docs/guides/extending.md)** documents `ROOST_ROOSTCTL` (with the
  Mac/Linux `roostctl` location split) and the lenient `activate`-stdout contract;
  `docs/reference/cli.md` gains a "Where `roostctl` lives" section + the env-var row.

## v0.0.9 — 2026-06-04

Extensibility release. Roost gains a **script-backed provider system**: drop an
executable in `~/.config/roost/providers/` (or add a `provider =` line to config)
and it appears as a dynamic, on-demand menu in a new **custom palette**
(⌘⇧E / Alt+Shift+E) — "open shed", "switch worktree", anything a script can list —
plus the CLI building blocks to act on a selection. See the new
**[Extending Roost](docs/guides/extending.md)** guide.

### Features

- **Script-backed providers + custom palette (#210)** — a `provider =` config entry,
  or an executable under `~/.config/roost/providers/`, is run on demand to populate a
  palette frame (`list`), then again to act on the choice (`activate`). Active-tab
  context is passed via env vars + a stdin JSON object, and `{items}` JSON is read back
  (drill-in supported). Surfaced via ⌘⇧E / Alt+Shift+E and a conditional
  "Custom Commands…" row in the command palette. One contract, both UIs.
- **`palette.present` IPC op (#210)** — a script hands Roost a list and blocks for the
  user's pick; the programmatic twin of the command palette. Exposed as
  `roostctl palette present` (items via `--items` or stdin).
- **Scriptable `tab open` + `tab list --json` (#211)** — `roostctl tab open` gains a
  trailing `-- <cmd…>` (run a command in the tab; closes on exit = hold=false), `--hold`
  (keep it open afterward, like `command = … hold=true`), `--after-tab <id>` (place the
  new tab next to that one), and `--focus`; `tab list` gains `--json`.
- **Non-actionable palette rows (#211)** — palette items can be marked non-selectable
  (`actionable: false`), so a provider's empty/disabled row (e.g. "No results") renders
  but can't be picked and leaves the palette open.

### Docs

- New **[Extending Roost](docs/guides/extending.md)** guide — CLI/IPC automation, the
  `command =` launcher, and the `provider =` protocol with bash / Python / TypeScript
  examples (incl. a complete "Open shed" provider). The **Configuration** reference is
  now linked in the nav, and `docs/reference/{cli,ipc}.md` document the new `tab.*` /
  `palette.*` surface.

## v0.0.8 — 2026-06-02

Terminal color-fidelity release. Fixes a palette bug that made every 256-color
TUI (vim, htop, lazygit — and notably **opencode over SSH**) render the wrong
colors, and adds OSC 4 palette-query replies so color-detecting TUIs read
roost's palette correctly.

### Features

- **OSC 4 palette-query replies (#208)** — roost now answers `OSC 4;Ps;?`
  palette queries from its live palette (reflecting any mid-session
  `OSC 4;Ps;rgb:…` set), mirroring the existing OSC 10/11/12 replies. opentui-
  based TUIs (opencode in local mode, among others) gate their color detection
  on the OSC 4 reply. `docs/reference/terminal-queries.md` documents roost's two
  reply-channel model — embedder-synthesized OSC colors vs. the planned
  libghostty `write_pty` device-query channel (tracked in #209).

### Fixes

- **256-color palette: cube + grayscale ramp (#207)** — roost's theme palette
  only populated the 16 ANSI colors; the xterm 6×6×6 color cube (16–231) and
  24-step grayscale ramp (232–255) were a flat placeholder (`#808080` gray on
  Mac, black on Linux). Because `set_color_palette` pushes the full 256-entry
  array, it also overwrote libghostty's correct compiled-in cube — so every
  `SGR 48;5;N` / `38;5;N` cell rendered the same wrong color. Most visible over
  SSH: **opencode in a shed** uses 256-color (because `COLORTERM` is unset
  there), so its `#080808` background rendered as `#808080` gray and was
  unreadable. Both UIs now compute the standard xterm-256 palette (cube levels
  `[0,95,135,175,215,255]`, grayscale `8+10·n`) as the base, with theme files
  overriding on top. Affects every 256-color TUI, locally and over SSH.

## v0.0.7 — 2026-06-01

Command-palette polish release. The palette gains a **Select Font…** picker that
writes your theme, font, and size choices back to config, and its root list is
reordered so the Theme/Font drill-ins lead. Tab titles now follow the working
directory on any shell (not just integrated ones), three GTK/Linux parity bugs
are fixed, and the e2e suite is reworked so a local run exercises exactly what
CI does.

### Features

- **Select Font… in the command palette + config write-back (#203)** — a new
  **Select Font…** entry in ⌘⇧P / Ctrl+Shift+P drills into a sub-frame of
  installed monospace families (arrow = live preview, Esc = revert, Enter =
  commit), mirroring the existing **Select Theme…** pattern. A curated
  programming-font list leads (JetBrains Mono, Fira Code, Cascadia, Iosevka,
  IBM Plex, plus platform natives like SF Mono / Menlo and DejaVu / Ubuntu
  Mono), filtered to what's actually installed. Theme, font family, and font
  size now persist to `~/.config/roost/config.conf` via a new atomic
  `RoostConfig.setKey` / `config::set_key` helper (tmp+rename, comment- and
  indentation-preserving). No new IPC ops — both UIs at parity.
- **Palette root leads with the drill-ins (#206)** — the command-palette root
  now orders Select Theme… → Select Font… → View Notifications → Clear All
  Notifications → the rest, on both UIs (the notification rows previously led).
- **GTK sidebar collapsed state persists across relaunch (#192)** — a
  ⌘B-collapsed sidebar on Linux now stays collapsed after quit + relaunch.

### Fixes

- **Tab title follows cwd on any shell (#202, closes #196)** — the model
  re-derives a tab's title from the cwd basename when the tab isn't
  user-titled, so the title tracks `cd` even on shells without the OSC 0
  integration loaded (Apple `/bin/bash` 3.2, `--norc`, etc.). Integrated shells
  still refine to the tilde-abbreviated full path on the next prompt
  (latest-wins, no flicker). `user_titled` is now persisted in the tab snapshot
  so a manual rename survives `cd` across relaunch; `derive_title("/")` matches
  across UIs.
- **Linux default tab is a non-login interactive shell (#192)** — match
  Ghostty's platform split (login shell only on macOS). A stray
  `~/.bash_profile` no longer shadows `~/.bashrc`, so the prompt, aliases, and
  color the user sees in their normal terminal load.
- **GTK palette overlay double-removal CRITICALs (#192)** — closing the command
  palette no longer logs a pair of `gtk_overlay_remove_overlay` assertion
  failures; `Drop` skips removal when `dismiss` already ran.

### Tests + CI

- **Trustworthy e2e suite — local == CI (#194)** — a skip now means only "this
  environment genuinely can't exercise this," never "the setup didn't work."
  Adds `ROOST_STATE_DIR` (both UIs) + Mac `UserDefaults` isolation
  (`ROOST_DEFAULTS_SUITE`) so the harness can own a fresh hermetic instance;
  `--roost-fresh` / `make e2e-{gtk,mac}-ci` make local runs exercise the same
  set CI does.
- **CI shell provisioning + flake hardening (#204, #205)** — provision zsh +
  Homebrew bash on CI and harden a shell-startup race; fix two local-env e2e
  flakes (OSC 52 PRIMARY selection + a sidebar layout-stall).

### Docs

- **Document the e2e harness flags, CI configs, and skip policy (#198).**
- **Abbreviate `$HOME` to `~` in the rooster-title recipe, portably (#199).**

### Release process

- **mise-aware cargo lookup in `update-version.sh` (#201)** — the release bump
  script resolves cargo through mise.
- **`create-github-app-token` v2 → v3 + app-id → client-id (#195, #200)** —
  bump the token action to v3 (Node 24) and switch to `client-id` to resolve
  the v3 deprecation.
- **URL-embedded token for the Sparkle appcast push (#122)** — fixes the
  appcast push that publishes the release.

## v0.0.6 — 2026-05-30

Mouse-aware terminals release. Strix and other mouse-driven TUIs now click,
drag, hover, and change cursor shape through Roost the same way they do under
ghostty, on both Mac and Linux. Plus two Mac sidebar fixes that ship the
behavior v0.0.5 was supposed to have, and a release-pipeline migration where
the same release-bot GitHub App now drives every cross-repo push (no more
per-pipeline PATs).

### Features

- **TUI mouse-tracking, focus, and OSC 22 cursor shape — Mac (#183) and
  Linux (#184)** — TUIs like strix that drive mouse-tracking modes (button,
  motion, SGR encoding), focus reporting, and OSC 22 cursor-shape changes
  now work end-to-end through Roost on both platforms. A new
  `tools/roosttest/test_mouse_tracking.py` suite enforces behavioral parity
  across `--roost-target mac` and `--roost-target gtk` so a regression on
  either side fails the matching CI job.

### Fixes

- **Mac sidebar holds its width on window resize** (#180) — re-fix of the bug
  PR #159 misdiagnosed (and that v0.0.5 still shipped). The Mac sidebar now
  owns resize redistribution directly via
  `splitView(_:resizeSubviewsWithOldSize:)`; the sidebar clamps to [160, 400]
  and the content view absorbs the window-resize delta.
- **Mac sidebar stays collapsed across quit + relaunch** (#181, #182) — fixes
  a pre-existing bug where ⌘B-collapsed sidebars silently re-opened on
  relaunch AND silently corrupted persistence by writing
  `RoostSidebarVisible=true` back to UserDefaults. PR #182 refactored the
  fix into a cleaner vision.md DL-11 shape: `selectProject(id:)` and
  `focusTab(tabID:)` are pure data mutators that never touch sidebar
  visibility; user-action call sites invoke `ensureSidebarVisible()`
  explicitly.

### Release process

- **Adopt `cc-plugins:release-workflows` convention** (#179) — roost is the
  first consumer of the new release framework: `scripts/release/update-version.sh`
  bumps Cargo.toml + Cargo.lock together (closes the Cargo.lock-drift bug
  class that produced the v0.0.5 mac-job failure), `RELEASING.md` documents
  the per-repo policy, two commits per release (`docs(changelog)` then
  `chore(version)`), and the skill flow is one command:
  `/release-workflows:release vX.Y.Z`.
- **Release-bot App as single cross-repo credential** (#191) — `apt-charliek`
  dispatch and the Sparkle appcast push now both authenticate via tokens
  minted from the `charliek-release-bot` App (scoped per-target via
  `actions/create-github-app-token`'s `owner` + `repositories`). The legacy
  `APT_DISPATCH_TOKEN` PAT is retired; the appcast bot identity reads from
  the action's `app-slug` output at runtime instead of being hardcoded so
  the App can be renamed without per-repo edits.
- **`sanity-check-app.yml` now verifies both roost AND `apt-charliek`** —
  multi-target shape catches App-install-on-target-repo mistakes before the
  next release tries to push there. Also fixes a latent `/user` 403 bug
  (installation tokens can't call `/user` — bot identity comes from the
  action's `app-slug` output).

### Tests + CI

- **Cross-platform mouse-tracking regression suite** (#183, #184) — every
  case runs against both UIs by default; gtk-skip markers from PR #183 were
  dropped in #184 once the GTK wiring landed.
- **Pipeline-helper consolidation** (#190) — shared `_wait_tab_attached` +
  `_drain_until` helpers extracted to `tools/roosttest/util.py` (CodeRabbit
  flagged the duplication across both mouse-tracking and OSC-pipeline tests).

## v0.0.5 — 2026-05-29

URLs, selection, and SSH-aware terminals release. URLs in the terminal are now
clickable on both Mac (⌘-click) and Linux (Ctrl-click), with OSC 8 hyperlink
ranges plumbed through the workspace; double-click selects words and
triple-click selects lines; `COLORTERM=truecolor` now follows you across SSH;
and `release.yml` cuts releases end-to-end — no more local post-release
script for the Sparkle appcast.

### Features

- **Click-to-open URLs** (#161, #171, #173, #175) — new `roost-url` crate
  detects URLs in the terminal viewport; OSC 8 hyperlinks honored; ⌘-click on
  Mac (#173), Ctrl-click on Linux (#175). Mirrored Swift implementation.
- **Word-on-double-click + line-on-triple-click selection** (#161, #176, #177)
  — both UIs; matches familiar terminal ergonomics; selection respects URL
  ranges where applicable.
- **`COLORTERM` forwarded across SSH** (#172) — new `ssh-env` shell feature
  injects `COLORTERM=truecolor` into the SSH environment so remote sessions
  render truecolor instead of dropping to 256-color.

### Release process

- **App-driven Sparkle appcast publish** (#178, closes #136) — `release.yml`
  EdDSA-signs the DMG and pushes `docs/appcast.xml` to main as the
  `charliek-release-bot` GitHub App, which is in `main`'s ruleset
  `bypass_actors`. Replaces v0.0.4's local `publish-appcast.sh` script
  (deleted). The release flow is now one command: `/release:release vX.Y.Z`.
- **`update-appcast.py` is now idempotent** — preserves the prior `pubDate`
  when replacing the same version, so workflow re-runs produce a clean
  no-op diff instead of a content-identical churn commit.
- **`main` migrated from classic protection to a ruleset** with `ci-success`
  required + the release-bot App in the bypass list.

### Fixes

- **Mac sidebar holds its width when the window is resized** (#159).

### Tests + CI

- **Test-only IPC ops** (#157) — `tab.feed_pty_bytes`, `tab.capture_pty_input`,
  `tab.dump_resolved` unlock new pytest coverage paths.
- **OSC pipeline end-to-end coverage** (#142, #145, #158) — real OSC bytes
  driven through the full pipeline in `tools/roosttest/test_osc_pipeline.py`.
- **Mac OSC drain tests** (#156) — exercise `TerminalView.appendBytes` drain
  with real OSC byte sequences.
- **URL + word selection fixtures** (`tests/url-fixtures/`,
  `tests/word-fixtures/`) — text-based fixtures covering schemes, unicode,
  trailing punctuation, multi-cell glyphs.

### Docs

- Spawned-shell env-vars table completed + cross-linked between the two
  shell-integration docs (#174).

## v0.0.4 — 2026-05-28

Rendering, selection, and clipboard release. Ghostty's sprite renderer is now
ported for crisp box-drawing + block elements; text selection survives
scrollback; copy-on-select + middle-click paste land for X11-style terminal
ergonomics; and clipboard image paste delivers a `.png` path to Claude Code on
both Mac and Linux. Plus OSC 10/11/12 + OSC 52 fixes that unblock codex's
theme detection and well-behaved program-initiated clipboard writes.

### Features

- **Sprite renderer ported from Ghostty** (#140) — box-drawing and
  block-element characters render crisp + cell-aligned instead of the font's
  fallback glyphs.
- **Three-state copy-on-select + middle-click paste** (#147) — selection
  auto-copies; middle-click pastes; matches X11/Linux terminal ergonomics.
- **Clipboard image paste → Claude Code** — clipboard images are written to a
  temp `.png` and the path is pasted as text, so `claude` picks the image up
  natively. Mac (#149) and Linux GTK (#153).
- **OSC 52 program-initiated clipboard writes** (#154) — programs (`tmux`, ssh
  forwards, etc.) can copy to the system clipboard via the standard escape.
- **Scrollback-aware selection** (#141, #146) — selection anchors to
  scrollback-stable coords, so highlights stay attached to the right text
  while you scroll through history.
- **Theme `bold-color`** (#142) — themes can override the color of bold cells.
- **`selection.*` / `clipboard.*` IPC ops** (#151) — let the pytest harness
  drive selection + clipboard from outside.

### Fixes

- **OSC 10/11/12 query replies** (#144, #145, #152) — answer terminal-color
  queries with libghostty's live colors so apps (e.g. codex) detect the
  dark/light theme correctly and stop rendering with a stuck gray bar.
- **SGR inverse + Mac two-pass rendering** (#139) — inverse / reverse-video
  cells render correctly under Mac's two-pass path; `Cell.style` exposed.
- **OSC 52 hardening** (#155) — drop oversized payloads, tighten selector
  parsing.

### Release process

- **`mac/scripts/publish-appcast.sh <tag>`** (#137) — local script (shed-style)
  replaces release.yml's bot-driven appcast push, which `main`'s branch
  protection rejected on v0.0.3. The maintainer runs it post-release; their
  `git push` lands the appcast entry cleanly. Closed #136.

### Docs

- Shell setup notes: `$SHELL` vs `which bash` and how to `chsh` to Homebrew
  bash (#138).

## v0.0.3 — 2026-05-27

Auto-update + shell-aware terminals release. Roost.app now ships with **Sparkle
2 auto-update** so future fixes reach users automatically (no more hand-delivered
DMGs), and the terminal is shell-aware: native cwd tracking, OSC 133
prompt/command marks, and shipped shell-integration scripts that auto-bootstrap
for bash and zsh. Plus the v0.0.2 clean-install crash fix and a batch of input
+ tab fixes.

### Features

- **Sparkle 2 auto-update** for the Mac app (#122, #128, #130) — EdDSA-signed
  releases via a GitHub Pages appcast; works under ad-hoc signing (no Apple
  Developer ID required). "Check for Updates…" in the App menu.
- **Native shell-cwd tracking** (#120) — new tabs inherit the active tab's
  working directory, read straight from the shell's PTY.
- **OSC 133 prompt/command marks** parsed by both VT scanners (#121, #127); a
  tab's `hookActive` run-state flips from them.
- **Shell-integration scripts shipped in-bundle** (#125, #126) — `roost.bash` /
  `roost.zsh` emit the env contract (cwd + prompt boundaries).
- **Auto-bootstrap** for bash (`--posix` + `ENV`) and zsh (`ZDOTDIR`) (#129,
  #132) — no `~/.bashrc` / `~/.zshrc` edits required.

### Fixes

- **Mac clean-install crash** (#116) — v0.0.2 crashed at launch on any machine
  that wasn't the build host because themes resolved through `Bundle.module`'s
  compile-time path. Themes now load via `Bundle.main` from `Contents/Resources`
  (and a deterministic CI guard catches this class of regression).
- **Default shell now spawned as a login shell** (#119) — picks up
  `~/.zprofile` / `~/.bash_profile` like a normal terminal.
- **Kitty / mouse-tracking input fixes** — scroll wheel encoded as button-4/5
  under mouse tracking; Ctrl+letter works under Kitty (unshifted codepoint set);
  Cmd-T / Ctrl-T new tab inherits the active tab's cwd.

### Tests + CI

- **Mac E2E is a required CI gate** for PRs and releases (#118).
- New pytest coverage in `tools/roosttest/` for new-tab cwd inheritance and
  shell integration (title, prompt, OSC 133 edges) (#131).

### Docs

- Shell-integration documentation rewrite (#126); dropped a stale
  `ROOST_PROJECT_ID` env reference never injected by the Rust/Swift port (#133).

## v0.0.2 — 2026-05-26

Programmability + automation release: the command palette and a growing set of
control ops are now driveable over IPC, with an end-to-end test harness
exercising both UIs — plus boot-reliability and Mac↔GTK parity fixes.

### Features

- **Command palette over IPC** — `palette.open` / `state` / `query` /
  `activate` / `dismiss` ops + `roostctl palette …`. Activating a row runs the
  same command its keybind would, so the palette is a scriptable command
  surface, not just UI.
- **`tab.dump`** — read a tab's terminal viewport as text (`roostctl tab dump`),
  the determinism backbone for content assertions.
- **`roostctl wait`** — block until a tab reaches a state / shows text / is
  gone; a no-`sleep` synchronization primitive for scripts and tests.
- **Command launcher** (`Cmd/Alt+Shift+T`), configured via `command =` lines
  (`label` / `run` / `title` / `hold` / `env`), and **Jump to Unread**
  (`Cmd/Alt+Shift+U`) — now on both UIs.
- **`ROOST_CONFIG`** environment variable to read config from an alternate file.

### Fixes

- Boot race: a tab opened via IPC during launch now reliably materializes
  (resync-on-subscribe) instead of never appearing.
- The notification jump + focus-tab now update the *core* active tab, so
  `identify` / `tab.focus` and the restored selection track what's on screen
  (both UIs).
- Mac↔GTK parity: one command-palette command set (`close_project`,
  `jump_to_unread`); Mac `tab.focus` switches the visible tab.

### Tooling & docs

- `tools/roosttest/` — pytest E2E driving a real UI over IPC, headless in CI on
  both platforms; plus the `tools/screenshot/` (visual) and
  `tools/input/linux/` (real-input) harnesses, mapped in `tools/README.md`.
- Reference docs for the IPC ops, CLI, keybindings, and config brought current;
  architecture + principles in `docs/development/vision.md`.

## v0.0.1 — 2026-05-23

First packaged release of Roost — a cross-platform (macOS + Linux) desktop
terminal multiplexer built around libghostty-vt, with multi-project workspaces
and notification routing for AI coding agents (Claude Code, Codex).

### Features

- Sidebar of projects, tabs per project, one terminal per tab.
- In-process workspace + PTY supervisor + JSON IPC server per UI process — no
  daemon. External tooling (`roostctl`, Claude hooks) talks newline-delimited
  JSON over a Unix-domain socket.
- OSC-driven tab titles, notification banners, and sidebar rollup for agent
  activity.

### Packaging

- macOS: `Roost.app` (Swift + AppKit) shipped as `Roost-0.0.1.dmg`, with the
  `roostctl` CLI embedded under `Contents/Resources/bin/`.
- Linux: GTK4 UI (`roost`) + `roostctl` shipped as `roost_0.0.1_amd64.deb` and
  `roost_0.0.1_arm64.deb`, auto-published to `apt.stridelabs.ai`
  (`sudo apt install roost`).
- The macOS DMG is ad-hoc-signed for now (Developer ID signing + notarization
  land in a follow-up once an Apple Developer account is available). Until then,
  open it via right-click → Open, or
  `xattr -dr com.apple.quarantine /Applications/Roost.app`.
