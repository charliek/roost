# Host sessions — roadmap

Status: **roadmap, in flight** — direction agreed 2026-08-26; HS-0,
HS-1 (plans 035 + 036), HS-2 (plan 037, PR #374), and HS-3 in full —
transport (plan 038, C1-C6) and bootstrap (plan 039, detect/install/
verify a remote `roost-session`) — have shipped; HS-4 is next. This
refines [`host-sessions.md`](host-sessions.md) (the
discovery note, which stays the rationale document) into an ordered set
of milestones with pinned design decisions. Per-milestone implementation
plans are written when a milestone starts; this file records the
sequence and the decisions those plans inherit.

Supersedes the discovery note's phase list where they differ. Where
this file's protocol summaries differ from the architecture doc,
**the architecture doc is normative**. The discovery note's "E8"
label for the Ghostty pin bump is retired: that work ("plan 032",
planned separately, superseding issue #333) bumps the pin to
ghostty main tip (`f2d5758f`, 2026-08-26) with the Zig 0.15→0.16
toolchain move — putting the snapshot API in reach.

---

## Direction (agreed 2026-08-26)

1. **The first personal payoff is Mac → remote SSH**: the iced app on
   the Mac as the client, `roost-session` on a Linux host (a real
   workbox, or a shed VM for testing). Native Roost chrome driving
   PTYs that live on another machine — the "no tmux/Herdr in the
   middle" workflow. Milestones are sequenced to reach that as
   directly as possible while each stays independently shippable.
2. **Default local behavior does not change.** In-process stays the
   default on both UIs for the whole roadmap. This is deliberate
   incrementality: if host sessions prove out, flipping local to a
   session backend is a later, explicit product decision (HS-5), not
   a side effect.
3. **Snapshot-first payload.** The attach payload is
   `ghostty-snapshot` (plan 032 landed the API; plan 034 / PR #371
   shipped the `roost-vt` wrapper). The wire stays kind-tagged
   (`vt` | `ghostty-snapshot`) so a formatter fallback remains
   possible, but the formatter path is not built.
4. **Localhost auto-reconnects after first opt-in; SSH stays
   explicit.** Once a user has connected `localhost`, relaunching the
   UI reattaches automatically — persist feels native. SSH hosts
   require an explicit Connect each launch (revisit in HS-4). Users
   who never connect a host see zero change (Hosts section hidden
   until the first connect).
5. **Roost.app (Swift) is untouched.** The Mac client for host
   sessions is the iced Mac app. Whether the Swift app ever grows a
   host backend is downstream of the mac-iced direction question in
   vision.md, not of this roadmap.
6. **The session server is Linux-only to start.** `roost-session`
   ships in the deb; the client is iced on Linux *or* Mac from the
   start (the Mac app participates as an SSH client at HS-3). A Mac
   `roost-session` build (Mac-local persist) is wanted eventually but
   comes after the SSH payoff, not before — it is nearly free in code
   (pure Rust, portable-pty) but adds Mac packaging/notarization
   scope.

---

## What the 2026-08-26 research pass verified

Beyond the discovery note (full detail lives in the note; this is
what changed or got confirmed by reading the actual code):

**Ghostty tip** (943 commits past our pin):

- The snapshot API is the designed attach primitive: encode a
  terminal to a stream of independently CRC32C-checksummed records
  laid out active-screen → CONTINUATION → `READY` → history pages
  newest-to-oldest → `FINISH`. The decoder hands back a renderable,
  typeable terminal at READY; history applies incrementally while
  live bytes flow. Bytes after FINISH are deliberately left for the
  embedding transport, and `SOURCE_OFFSET` marks the boundary. The
  CRC32C is integrity only — the format carries no authentication at
  this pin, so the transport (localhost socket perms, SSH at HS-3)
  remains the only thing standing between a session and a forged
  stream.
- **Format v1 has no binary-compatibility promise** — the decoder
  rejects any other version. Client and server must run the same
  libghostty build. Localhost: free (same package). SSH: the
  bootstrap must exact-match versions (Herdr requires this anyway).
- **Continuation tracking must be enabled at terminal construction**,
  before the first PTY byte, or a snapshot taken mid-escape-sequence
  fails. Server-construction detail, pinned in D5.
- **Resize during an attach silently drops all remaining history
  pages.** Serializing attach against resize was considered and
  rejected: instead, attach carries the client geometry so the
  server resizes *before* encoding, and the client withholds
  further resize until the snapshot finishes — accepted loss only
  in the rare takeover race (architecture §4–5).
- Kitty image state does not survive a snapshot (nor the formatter).
  Documented limitation for the whole roadmap.
- The formatter (VT dump) exists at the *current* pin and a
  whole-screen dump avoids the GridRef UB hazard; it's ~60 lines if
  ever needed as a fallback. Active screen only, no continuation.

**Roost seams** (better than the discovery note assumed):

- `roost-engine` is already headless-capable: no UI dependency, no
  `roost-vt` dependency, no main-thread assumptions, and
  `crates/roost-engine/tests/ipc_dispatch.rs` already boots
  Workspace + PtySupervisor + IpcServer with no UI — a miniature
  `roost-session`. The bootstrap is ~35 lines; `ROOST_SOCKET` is a
  constructor argument.
- Three real gaps, all scoped: (1) the engine owns no libghostty
  `Terminal` — the UI does; a server-side per-tab Terminal is the
  largest net-new chunk. (2) `roost-ipc` has no push/streaming — the
  connection loop is strictly serial request→response and the write
  half is loop-owned (`events.subscribe` returns `not-implemented`;
  note `docs/reference/ipc.md` claims otherwise — doc-drift fix to
  land with HS-0). (3) No fence primitive: PTY broadcast
  (`pty.rs`, capacity 256) has no sequence numbers.
- The `TabBackend` seam is unstarted but narrow: `attach`,
  `send_input`, `send_resize`, `has`, `foreground_cwd` — 14 call
  sites. `TerminalWidget` already consumes only a `TerminalSnapshot`
  value and needs no changes.
- The 16 MiB JSON frame cap is ~12 MiB effective after base64 — a
  reason the data plane is binary-framed, not JSON (D3).
- The feature-gated `facade.rs` (#286) is ~80% of a session command
  surface but carries no PTY stream and stays frozen; `roost-session`
  is built from the engine directly, not by reviving the facade.

**Herdr** (operational checklist, not code): the specific practices
this roadmap adopts are pinned in D6–D8 below. The headline lesson:
Herdr's server renders frames because its client is a dumb TUI; our
client is a full libghostty VT (the Superlogical model), which is
exactly why we need the snapshot payload Herdr doesn't.

---

## Pinned design decisions

### D1 — Client and products

Iced is the only client, on both platforms. The Mac iced app is the
Mac client. The Swift app, GTK (removed), and any web/mobile client
are out of scope. Default launch of the iced app is in-process and
byte-identical to today until the user connects a host.

### D2 — Two planes on the session socket

- **Control plane**: existing `roost-ipc` newline-JSON op set on the
  session's own socket (new `BundleProfileKind::Session` or
  equivalent; distinct path from the UI's `roostctl` socket). An
  `identify`-style version/protocol handshake happens here **before**
  any attach — an old server must produce a clean error, never a
  garbled binary frame (Herdr's stable-JSON-socket lesson).
- **Data plane**: one additional, bidirectional connection per
  attached tab, upgraded from a JSON handshake to length-prefixed
  binary framing: snapshot chunks (Ghostty's own READY record is
  the readiness marker — no second one) interleaved with seq-tagged
  live PTY frames server→client, and unacknowledged ordered
  input/resize frames client→server. No base64, no 16 MiB
  single-frame constraint. Exact handshake and frame shapes are
  normative in the architecture doc (§4.3), including
  resume-from-seq.
- Unknown version or kind fails the attach cleanly. No half-applied
  snapshot.

### D3 — Attach contract

Superlogical/discovery-note shape, made concrete:

1. `tab.attach` carries the client geometry; the server resizes the
   tab, then the attach forwarder subscribes to the tab's tee, then
   the tab task encodes the authoritative Terminal, recording the
   fence sequence.
2. Client receives snapshot frames → decodes → Ghostty's `READY`
   record: renderable, typeable, scrollable. From here input and
   resize flow as ordered frames on the same data connection.
3. Live tee: seq-tagged raw PTY frames; the client applies exactly
   `fence+1, fence+2, …` — a gap or duplicate is desync. No pause
   of the PTY drain required.
4. History pages apply in the background under bounded weighted
   scheduling (live frames lead but cannot starve FINISH).
5. Desync recovery = resume-from-seq off the server's replay ring
   when possible, else a fresh attach with backoff; the UI keeps
   the old hydrated Terminal until the replacement reaches READY.
   Takeover semantics per D8 (lease-enforced).

### D4 — Fence = sequence numbers

`PtyOutputEvent` gains a per-tab monotonically increasing sequence
number (a per-output-record ordinal, generation-scoped on the wire —
architecture §4.3) at the supervisor. This is the fence primitive
for attach, and it also makes broadcast-lag detection precise.
Landing this in HS-0 is safe and invisible (source changes at the
enum's match sites, no behavior change); the HS-1 plan carries a
test pinning fence semantics across the seq-assignment move into
the tab task.

### D5 — Server-side Terminal lives in `roost-engine`, feature-gated

A per-tab authoritative `roost-vt::Terminal` behind an engine feature
(working name `server-vt`) so the default engine build stays
ghostty-archive-free. The session drain feeds it and tees to
subscribers. Construction: continuation tracking on before the first
byte; scrollback limit matching the UI's policy; sized from the last
attached client (D7). OSC/agent detection continues to run where the
VT lives — which for a host is `roost-session`
(see [`agent-watching.md`](agent-watching.md); still not coupled).

### D6 — `roost-session` binary and lifecycle (Herdr checklist)

Separate binary, packaged beside `roost`/`roostctl` in the deb
(`nfpm.yaml`); the Mac bundle ships it only when Mac-local sessions
land (post-HS-3). Lifecycle practices adopted from Herdr:

- Daemonize with a single fork + `setsid()`; parent exits
  immediately. Launch cwd passed as a consumed-once env hint, not
  inherited.
- Socket handling: connect-probe before unlinking a stale socket;
  record `(dev, ino)` at bind and only unlink an owned socket on
  shutdown; `0600` socket in a validated user-owned `0700` runtime
  dir (symlinked/world-writable parents rejected); **plus a
  same-UID peer check** (`getpeereid`/`SO_PEERCRED`) that today's
  `IpcServer` lacks — mode bits are not enough once the socket is
  SSH-forwarded. Never TCP.
- Debug builds use a distinct profile directory so a dev
  `roost-session` can never attach to the real one.
- The server never self-exits. Stop is an explicit op; the stopping
  client polls until the socket is actually unreachable.
- Reuse `single_instance.rs` flock locks as-is.
- One default session per host to start; named sessions deferred.

### D7 — Headless PTY environment

- **Detach does not resize.** Existing PTYs keep the last attached
  size (no SIGWINCH storm into TUI agents). Tabs created while
  detached get a configured headless default (e.g. 120×40).
- `TERM`/`COLORTERM` are already forced by `pty.rs` — unchanged.
  Session PTYs get `ROOST_SOCKET` pointing at the session socket, so
  Claude hooks work unmodified.
- Client-local side effects route to the attached client only:
  OSC 52 clipboard writes (with Herdr-style allow-list limits), bell,
  notifications. With no client attached they are dropped and logged.
  OSC color queries are answered by the server VT's own palette; the
  attached client's theme seeds it (replacing the UI-theme seed used
  in-process). Remote clipboard/image-paste policy is an HS-3
  decision.

### D8 — Session semantics

- **Disconnect ≠ Stop session**, distinct ops and distinct UI verbs
  from day one. Closing the window is Disconnect.
- Second connect to the same session: **takeover** (MVP), enforced
  by a random per-connect lease, not a self-declared client id —
  the stale client loses all its connections and its ops error
  cleanly (architecture §4.1). Independent per-client viewports are
  HS-4.
- Localhost auto-reconnects on launch once opted in; SSH is explicit.
- Local in-process tabs and host tabs are separate worlds; no
  migration in the MVP.
- Saved hosts (`{id, label, target, last_connected}`) live in the
  *client's* `state.json` (additive `#[serde(default)]` field).
  Remote project/tab layout lives in the host's own `state.json`.

### D9 — SSH transport and bootstrap (HS-3)

**Status: shipped — the transport slice as plan 038 (PR #377), the
bootstrap slice below as plan 039.** The bullets below describe
bootstrap as originally scoped; see
[`docs/development/host-sessions.md` → Bootstrap: install/upgrade over SSH](../docs/development/host-sessions.md#bootstrap-installupgrade-over-ssh)
for what actually landed. The shipped rule is sharper than "version and
protocol must both match" below: the **install** rule compares all
three of `app_version`, `session_protocol` and `libghostty_build`
byte-for-byte, deliberately stricter than the unchanged runtime attach
gate (protocol + payload kind + build, no `app_version`) — see that
section for why the two are allowed to disagree.

- **Stdio-mux** (Herdr's shape): local UDS bridged to a far-side
  `roost-session client-bridge` process over ssh stdin/stdout — one
  `ssh -T` exec **per accepted connection**, shared over a private
  per-host `ControlMaster` (topology normative in architecture
  §10). Avoids remote socket-path management and works everywhere;
  UDS forwarding remains a possible alternative if the bridge
  disappoints.
- HS-3 splits into two slices: **transport first** (client ↔ a
  *preinstalled* remote `roost-session`, automated over local pipes
  + shed sshd), **bootstrap second** (detect/install/verify). The
  automated transport tests are acceptance-blocking; a manually
  exercised real box is evidence, not acceptance.
- Bootstrap checklist (Herdr's, adopted): platform detect via
  `uname`; candidate paths (PATH via login shell then `/bin/sh`,
  `~/.local/bin`, brew/nix locations); **version and protocol must
  both match** (mandatory anyway given snapshot v1's same-build
  rule); install by streaming the binary to `dest.tmp.$$` via `tee`,
  `chmod`, `mv`, then re-verify; warn if `~/.local/bin` is not on
  PATH — never edit dotfiles; non-interactive runs fail rather than
  mutate a host.
- Generated ssh config **includes the user's config first** (their
  keepalives win), fallback `ServerAliveInterval`; private per-host
  `ControlMaster`, torn down with an explicit `ssh -O exit`.
- Cross-platform bootstrap (Mac client → Linux host) needs a release
  artifact per target — the deb already ships `roost-session` by then
  (D6); the bootstrap can also download the matching release asset.

### D10 — Out of scope for the whole roadmap

Herdr/tmux as the host runtime; splits; web/iOS; disk scrollback
persistence (secrets — Herdr ships it off-by-default for a reason;
revisit only post-HS-4); screen-manifest agent detection (own track);
Swift host backend; flipping the local default (HS-5 is a placeholder
decision point, not scheduled work).

### D11 — Hosts UX

Pinned with Charlie in the plan 037 planning session (approved mockups
archived beside that plan); see `docs/development/vision.md`'s DL-18
for the summary and `docs/guides/host-sessions.md` for the user-facing
shape.

- **Sidebar = per-host sections.** Zero saved hosts renders exactly
  today's single "PROJECTS" sidebar (D8's zero-change rule); the first
  saved host splits it into one band per host — LOCAL first, then each
  saved host by label — each with a connection dot (green connected /
  grey disconnected / amber connecting or needs-restart) and a
  right-aligned agent rollup. A disconnected host's rows stay listed,
  dimmed and non-interactive, with an inline "↻ Reconnect" — the
  session still holds those shells.
- **Palette owns every host verb.** No host menu: `Add Host…`,
  `Connect Host: <label>`, `Disconnect Host: <label>`,
  `Stop Session: <label>` (connected-only, confirmed),
  `Remove Host: <label>` (disconnected-only), `New Project on… <label>`
  — one row per (verb, host) pair, appearing only where it applies.
  Add Host is the one flow needing free text: a small modal dialog
  (Name + Socket) validating by dialing `session.identify` before it
  saves.
- **The Mac gate.** No macOS `roost-session` exists, so the Roost-Iced
  Mac client hides the `localhost` surface (no seeded connect row, no
  launch auto-reconnect, no spawn-if-missing) — refining rather than
  contradicting the "no visible dead end" rule above. `Add Host`
  **stays** on macOS: an `ssh -L` forward to a Linux host is the
  feature's Mac→Linux payoff, not a dead end.
- **Creation follows context.** `⌘N`/`Alt-N` and "+ New Project"
  create on the selected project's host; `⌘⇧N`/`Alt-Shift-N` opens a
  "New Project on…" picker (connected hosts + LOCAL); `⌘T`/`Alt-T`
  never lands a tab on a different host than its project.
- **Navigation is one global ring**, crossing host boundaries and
  skipping disconnected sections, with no new bindings; agent surfaces
  (sidebar rows, the agents palette, notifications) are host-blind —
  `project · host` is the only tell.
- **Takeover** freezes the displaced window's last frame under a
  banner ("Reconnect here" = takeover back); the **upgrade flow** on a
  build/protocol mismatch offers "Restart session" only where the
  client can run it (localhost), and a docs pointer otherwise (a
  remote session restarts on its own machine).
- **Disconnect ≠ Stop** (restating D8 for the UI layer): closing the
  window disconnects every host; Stop is explicit and confirmed.
- **Iced-only.** The Swift app is untouched; `roostctl host *` against
  its socket answers `unknown-op`, permanently — not a gap.

---

## Milestones

Each is independently shippable and useful. Vision.md's DL revisions
(the "no daemon" and "no remote IPC" non-goals becoming host-session
exceptions) land with the HS-1 implementation plan, not before.

### HS-0 — Seams (no user-visible change)

- `TabBackend` in `roost-iced` so `TerminalTab` takes bytes in and
  writes input/resize out without naming `PtySupervisor` — cut at
  the *real* surface, including the backend-dependent policies
  (reply-drain, OSC-scan mode, theme reseed, test hooks —
  architecture §9), not just the five obvious methods.
- **Host-qualified identity**: `TabKey`/`WorkspaceKey` (host +
  id + server generation) through the feed, tab map, and events —
  bare `i64` ids collide across hosts and restarts, and retrofitting
  this later means redesigning every event API twice (architecture
  §9).
- Sequence numbers on `PtyOutputEvent` (D4).
- Attach payload `kind` + protocol-version types in `roost-ipc`;
  event-envelope revision-batch shape (architecture §4.2).
- `hosts: Vec<HostSnapshot>` in `state.json` — client-side state,
  additive, default empty. The Swift `state.json` mirror is a
  mechanical schema addition per the durable-boundary rule ("Swift
  untouched" means no behavior, not no schema twin).
- Same-UID peer check + socket-directory hygiene in `roost-ipc`'s
  server (used by the session profile; UI sockets unchanged).
- Housekeeping: fix the `events.subscribe` doc drift in
  `docs/reference/ipc.md` (documenting it as not-implemented until
  HS-1 ships it).

**Acceptance:** default iced indistinguishable from today; full CI
green; no new binary — **plus positive tests, not just
non-regression**: seq monotonicity unit test, peer-check
accept/reject test, `hosts` round-trip through both the Rust and
Swift decoders, and the existing e2e suite passing *through* the
`TabBackend` seam (proving equivalence, not absence).

*Parallel-safe with plan 032* (touches `roost-iced`/`roost-engine`/
`roost-ipc`; plan 032 touches `roost-vt`/`build.sh`/`mac`). Run in a
worktree on a topic branch regardless.

### HS-1 — Headless `roost-session`, driven by `roostctl` (no UI)

**HS-1 executed as two plans, not one.** HS-1a (plan 035, shipped) is the
lifecycle + control plane slice: the new crate + binary, daemonize +
readiness + locks + stale-socket handling, awaited shutdown, headless
hydration + per-tab OSC drains, `session.identify`/`session.stop`,
`events.subscribe` implemented **provisionally** (no lease yet — see
[`ipc.md`](https://github.com/charliek/roost/blob/main/docs/reference/ipc.md#session-sockets)
for the full deviation list), `roostctl session start|stop|status`, the
Linux `.deb` shipping `/usr/bin/roost-session`, and a dedicated
`e2e-session` pytest lane + required CI job. HS-1b (next plan) is the
data plane: server-side Terminals (`server-vt`, D5), the attach stream
(D2/D3), connect leases (D8) — which turns HS-1a's leaseless
`events.subscribe` into a breaking change — and session-side
`tab.dump`/`dump_resolved`. **HS-1b shipped (plan 036)**: the
server-VT tab task, the binary attach data plane (snapshot payload,
resume-from-seq, INPUT/RESIZE), `session.connect` leases with takeover,
and headless dumps + terminal replies — which closes the leaseless-
`events.subscribe` deviation (now lease-gated,
`SESSION_PROTOCOL_VERSION: 2`) along with HS-1a's other two. The
bullets below describe HS-1 as originally scoped; the split above is
what actually shipped.

- New crate + binary: engine bootstrapped headless (the
  `ipc_dispatch.rs` pattern), session socket profile, daemonize +
  readiness + locks + stale-socket handling, awaited shutdown (D6,
  architecture §8).
- Server-side Terminals (`server-vt` feature, D5), continuation
  tracking on, headless size policy (D7), bounded tab-task pipeline
  + replay ring (architecture §3).
- The attach stream itself: bidirectional data-plane connection,
  snapshot payload, READY, seq-tagged tee, input/resize frames,
  resume-from-seq, connect leases (D2/D3/D8). The client-side
  `roost-vt` snapshot wrapper **already shipped ahead of HS-1**
  (plan 034, PR #371: encode + streaming `SnapshotDecoder` with a
  36-test contract battery), so HS-1 consumes it rather than builds
  it; the Rust integration client proving decode fidelity over the
  real attach stream remains HS-1 work.
- `events.subscribe` implemented (revision batches + resync
  contract test, architecture §4.2) — HS-2's sidebar depends on it.
- Explicit stop op. `roostctl` targeting rules for the second
  socket. Session-side `tab.dump`/`dump_resolved` served from the
  server Terminal.
- **Validated end-to-end by a new roosttest lane speaking the
  protocol from Python** (plus the Rust integration client for
  decode), byte-level, before any UI exists — see architecture §12
  for the contract-test list, CI path-filter wiring, and the perf
  budget.

**Depends on:** plan 032 PR A merged (snapshot API) — satisfied, and
the `roost-vt` wrapper shipped (plan 034, PR #371). The kind-tagged
wire keeps the ~60-line formatter fallback possible as a swap, not a
redesign, should a pin bump ever break the format.

**Acceptance:** start session → open tabs via `roostctl` → agents
keep running with no UI anywhere → `tab.dump` shows live state →
attach-stream contract (fence, READY ordering, no lost/duplicated
bytes around the fence) proven by the pytest lane.

### HS-2 — Iced attaches to localhost (first user-visible ship)

**Shipped (plan 037).** The bullets below describe HS-2 as originally
scoped; see [D11](#d11--hosts-ux) for the UX decisions pinned during
planning and `docs/development/host-sessions.md` for what actually
landed (`HostConn`'s three-connection-per-host topology, the
attach-on-focus data path, `tab.effect` + `session.set_theme` on the
server side, and the takeover/upgrade lifecycle) — implementation
deviated from this sketch in a few recorded places (notably: the
terminal swaps at FINISH rather than READY, and `TabBackend` gained no
app-scoped `Host` variant — the real seam turned out to be
`TabHandleKind::Host` plus `HostConnSet`). One gap recorded rather than
fixed: `roost-session`'s headless workspace defaults
`window_focused = true` with no op to correct it, so `notification.fired`
never fires for whichever tab a client has attached to — tracked as
HS-3 follow-up. **Closed by HS-3's transport slice**
(`session.set_focus`, plan 038 C6) — see the open-questions entry
below.

- Palette: Connect host → `localhost`; spawn-if-missing then attach.
  Hidden on the Mac build until a Mac server exists (no visible
  dead end — architecture §9).
- Hosts section in the sidebar (hidden until first connect);
  selecting the host context-switches projects/tabs.
- Attach visible tabs through `TabBackend::Host`; existing
  `TerminalWidget` renders them unchanged.
- Disconnect on quit; explicit Stop session; takeover on second
  connect; auto-reconnect localhost on next launch (D8).
- **The upgrade flow**: on `session.identify` build mismatch (every
  package upgrade after opting in), a first-class "Restart session
  (ends shells, keeps layout)" flow — architecture §4.4. This is
  the feature's most common failure mode, so it ships with the
  feature, with an acceptance test.
- Hooks: session PTYs carry the session's `ROOST_SOCKET`.

**Acceptance:** the discovery note's Phase-1 criterion — close Roost,
agents keep running, reopen, same shells, still looks like Roost —
plus exit tests for the D8 behaviors HS-2 ships: takeover, stop vs
disconnect, localhost auto-reconnect, and the upgrade "Restart
session" flow. Verified on Linux (shed or native). The Mac iced app
builds the same client code but has no local server to talk to yet —
its first live use is HS-3.

### HS-3 — SSH remote host (the target payoff)

**Shipped in full: transport (plan 038, C1-C6) and bootstrap (plan
039).** The bullets below describe HS-3 as originally scoped; see
[`docs/development/host-sessions.md` → Transport: SSH hosts](../docs/development/host-sessions.md#transport-ssh-hosts)
and
[`docs/development/host-sessions.md` → Bootstrap: install/upgrade over SSH](../docs/development/host-sessions.md#bootstrap-installupgrade-over-ssh)
for what actually landed and
[`docs/guides/host-sessions.md`](../docs/guides/host-sessions.md#adding-a-remote-host-over-ssh)
for the user-facing shape. What the transport slice shipped: D9's
stdio-mux transport (a per-connection `ssh -T` exec over a shared
private `ControlMaster`, `BatchMode=yes` — key/agent auth only, no
password/2FA this slice), the target classifier (`workbox` /
`user@host` / `ssh://user@host:port` in the palette's Add Host dialog
and `roostctl host add --target`), the six-family classified-failure
surface (changed/unknown host key, auth, no session, `roost-session`
not found, transport — reaching the sidebar band and an attended
attempt's toast), `--verify`'s mux-less probe over SSH, and
`session.set_focus` (closing the HS-2 "session doesn't know what a
client is looking at" gap, folded into that plan as C6). What the
bootstrap slice then shipped: the eight-rung candidate ladder (one
definition generating both the probe and the exec chain), the probe
that classifies `Compatible` / `Mismatch` / `Missing` over a job-scoped
`ControlMaster`, the consent-gated install (staged, verify-before-
commit, incumbent backed up and restored on failure) sourced from a
local override, this client's own sibling binary, or a checksum-
verified release-asset download, the install→stop→await-gone→start→
verify upgrade order, and the in-app offers on the NotFound/NoSession/
NeedsRestart dialogs. SSH auto-reconnect remains manual, as originally
scoped below (HS-4).

- Palette accepts `workbox` / `user@host` / `ssh://…`; D9 transport
  and bootstrap. **Both shipped** — see above.
- Disconnect vs Stop unchanged over SSH; reattach command surfaced
  on disconnect.
- Remote clipboard policy decided (OSC 52 to the attached client's
  machine; image paste deferred if needed). **Shipped as scoped**:
  OSC 52 writes forward to whichever client holds the tab's lease,
  applied under that client's own `clipboard-write` setting; OSC 52
  *reads* stay parser-level default-deny on every path, local or
  remote — reading is the more sensitive direction over SSH, not
  less, so there's no SSH-specific carve-out. Remote image paste
  remains deferred (unchanged open question below).

**Acceptance:** the automated lanes are the gate — the pipes-based
bridge lane and a shed-sshd lane (`ssh <shed-name>@localhost -p 2222`
— the shed platform's own sshd, username = shed name; the existing
`linux-test` harness) covering transport, auth/host-key
failure surfaces, connection loss, and bootstrap verify. On top of
that, the live criterion: Mac iced app connects to the shed VM —
native tabs drive PTYs running in the VM; quit the Mac app, agents
in the VM keep running; reconnect shows the same shells. A real
Linux box run is supporting evidence, not the acceptance. **The
automated lanes are met by both slices** — the transport's pipes-based
bridge lane in the required `session-e2e` job plus a fake-ssh UI lane
in the Linux iced e2e cells (plan 038 C5); the bootstrap slice's own
`cargo test -p roost-ipc` suite (hermetic fake-ssh `run-remote` mode,
no sshd, no network) and its `test_host_bootstrap.py` UI lane riding
the same fake-ssh cells (plan 039 C3/C6). CI still has no sshd, so the
shed-sshd checklist and the live Mac-to-shed criterion (now including
an end-to-end install against a shed-built binary) are exercised as
manual verification for both plans.

### HS-4 — Make the primitive feel inevitable

Only after HS-3 is boring — and explicitly a *bucket of separate
follow-on plans*, not one milestone (each item below has its own
architecture and risk): independent per-client viewports (replacing
takeover); auto-reconnect for chosen SSH hosts; "Move tab to host";
a Mac `roost-session` build for Mac-local persist (cheap in code,
real packaging scope); second-window-as-second-client if the
multi-window question becomes live. Porcelain-heavy; sequenced by
usage, not up front.

### HS-5 — Local default flip (decision point, not scheduled)

Launch iced as a client of a local session always, with
`--ephemeral` as the escape hatch. Explicitly a product decision to
be made after living with HS-2/3; the HS-0 seams exist so it is a
default-backend change, not a rewrite.

---

## Coordination

- **Plan 032** owns the pin bump and files the snapshot-adoption
  issue (its D8) that HS-1 picks up. HS work references plan 032,
  not "E8". The snapshot wrapper in `roost-vt` waited for 032's PR A
  to keep `roost-vt` churn serialized and **has since shipped**
  (plan 034, PR #371 — the attach half of #363; the persistence
  half stays open).
- All HS implementation happens on topic branches in **worktrees**;
  no HS PR edits `vision.md` except the HS-1 plan's DL revision
  commit.
- The discovery note stays as rationale; if a later pass changes a
  recommendation, update both files.

## Open questions (deferred, with owners)

- Remote image paste + OSC 52 *read* over SSH → **partially resolved by
  the HS-3 transport slice (plan 038)**: OSC 52 *read* stays
  parser-level default-deny on every path (settled, not deferred —
  reading is the more sensitive direction over SSH, if anything).
  Remote image paste remains genuinely deferred — still open, tracked
  as a follow-on to HS-3.
- A session's own attention not reaching an attached client
  (`window_focused` always `true`, no way to correct it — HS-2's
  recorded gap) → **resolved by the HS-3 transport slice**:
  `session.set_focus` (plan 038 C6) closes it for a current session; it
  remains true only against a session too old to serve the op, which
  degrades harmlessly to HS-2's behavior. See
  [`docs/reference/ipc.md`](../docs/reference/ipc.md#sessionset_focus)
  and [Known limitations](../docs/development/host-sessions.md#known-limitations).
- Named sessions per host (`workbox:agents`) → after HS-3, if wanted.
- `systemd --user` / launchd supervision of `roost-session` (vs
  plain daemon) → HS-2/3 hardening; plain daemon first.
- ~~Whether the Mac iced app ships host-sessions before general
  iced-on-Mac parity messaging~~ **Resolved by events**: the Mac iced
  build shipped the client surface with HS-2 (localhost hidden per
  D11's Mac gate) and its SSH payoff with HS-3 slice 1 — host
  sessions ride the experimental Mac app without waiting on parity
  messaging; the Swift app stays untouched.
