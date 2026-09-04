# Host sessions — roadmap

Status: **roadmap, in flight** — direction agreed 2026-08-26; HS-0,
HS-1 (plans 035 + 036), HS-2 (plan 037, PR #374), HS-3 in full —
transport (plan 038, C1-C6) and bootstrap (plan 039, detect/install/
verify a remote `roost-session`) — and HS-4a (SSH auto-reconnect,
plan 040, PR #380) and HS-4b (Mac-local persist, plan 041, PR #389)
have shipped. **Next: the pre-release follow-ups** — see the
[sequencing call](#sequencing-toward-the-first-release-proposed-2026-09-01).
Both platforms now run a local session; what is still Linux-only is
being an SSH *host* (HS-4c). None of it is released yet —
the first release carrying host sessions gates on the
[pre-release follow-ups](#tracked-follow-ups-pre--post-initial-release)
plus the sequencing call recorded under
[HS-4](#hs-4--make-the-primitive-feel-inevitable). This
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
   **Revisited by HS-4 slice 1 (plan 040, shipped):** the rule above
   stands exactly as written, narrowed to *launch* — an SSH host still
   never auto-connects when Roost opens. What HS-4 slice 1 changed is
   the *mid-session drop*: a saved SSH host that has actually reached
   `Connected` once now auto-retries on its own capped backoff (1s
   base, 30s ceiling, giving up and settling after 10 attempts) before
   ↻ Reconnect becomes the only path back — the same shape `localhost`
   already had for a drop. See
   [`docs/development/host-sessions.md`](../docs/development/host-sessions.md#the-leasetakeover-lifecycle)
   for the shipped rule.
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
   **Shipped (2026-09-01) as HS-4b** — this bullet's "Linux-only to
   start" is now history: the Mac bundle carries the daemon, and both
   platforms run a local session. What remains Linux-only is being an
   SSH *host*. Discovery
   verified the "nearly free in code" claim directly: `cargo check -p
   roost-session` passes on macOS, `BundleProfile`'s session paths
   already carry a Mac branch (`~/Library/Caches/RoostSession/`), and
   the spawn ladder's sibling-of-exe rung finds a bundled binary with
   zero code change. The remaining scope is exactly the
   packaging/notarization named above, plus un-gating two `cfg!` sites
   and widening CI — enumerated under HS-4b.

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
(`nfpm.yaml`); the Mac bundle ships it at HS-4b (Mac-local persist),
at `Contents/MacOS/roost-session` beside the app binary so the
existing sibling-of-exe discovery rung finds it. Lifecycle practices
adopted from Herdr:

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
  **Narrowed to launch, and stands there unchanged** — an SSH host
  still never auto-connects when Roost opens. **HS-4 slice 1 (plan
  040, shipped)** revisited the *mid-session drop* instead: a saved
  SSH host that reached `Connected` at least once now auto-retries a
  drop on its own capped backoff, settling after 10 attempts, exactly
  as localhost already did. See
  [`docs/development/host-sessions.md`](../docs/development/host-sessions.md#the-leasetakeover-lifecycle).
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
  feature's Mac→Linux payoff, not a dead end. **HS-4b lifts this
  gate** by shipping the Mac `roost-session`; the gate's actual
  implementation (one `VerbPolicy` value plus one redundant `cfg!` in
  `reconnect_saved_hosts`) is enumerated there. Dialing an
  already-running session was never gated, deliberately.
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

Explicitly a *bucket of separate follow-on plans*, not one milestone —
each slice below has its own architecture and risk, and each gets its
own implementation plan when it starts. Enumerated and sequenced
2026-09-01; the sequence is a decision on today's information, made
concrete on purpose (we would rather revise a stated order on user
feedback than keep it ambiguous), and only HS-4b's "next" is firm.

#### HS-4a — SSH auto-reconnect (plan 040, PR #380) — SHIPPED

Every SSH host gets it once its connection has actually worked — not a
per-host opt-in (see D8's amended rule). Bounded ladder (1s base, 30s
cap, settles after 10 attempts), never auto-connects at launch, never
auto-spawns a session, never retries a changed host key or a failure
classified from a truncated stderr tail. #379's unbounded drains fixed
alongside. Its outliving follow-ups (#381–#388) are mapped under
[Tracked follow-ups](#tracked-follow-ups-pre--post-initial-release).

#### HS-4b — Mac-local persist (plan 041, PR #389) — SHIPPED

The localhost story Linux got at HS-2, on the Roost-Iced Mac app:
palette **Connect Host: localhost** (seeded row, spawn-if-missing),
quit Roost, relaunch, same shells — plus, for free, every Mac becomes
a *dialable* host, since dialing a running session was never gated.

**Scope nuance, stated plainly:** persist covers tabs created under
the localhost host section. Ordinary local tabs stay in-process and
still end with the app — making persist the default for everything is
HS-5, still an explicit future decision. Default behavior is untouched:
zero saved hosts renders today's UI byte-identically on both platforms.

Discovery (2026-09-01) verified the code side is essentially done:
`cargo check -p roost-session` passes on macOS; the daemonize sequence
is pure POSIX (the engine's one `/proc` read already has a
`proc_pidinfo` branch); `BundleProfile` session paths have a tested Mac
branch (`~/Library/Caches/RoostSession/roost.sock`, `RoostSessionDev`
for debug builds — a dev daemon cannot collide with a real one); and
`locate_session_binary`'s sibling-of-exe rung finds a bundled binary
with no code change. The actual work, pinned for plan 041:

1. **Un-gate — two sites, not one.** Flip `VerbPolicy::current()`
   (`roost-ui-model/src/host_verbs.rs:72`, the designed seam every
   consumer reads) **and** delete the redundant independent
   `cfg!(target_os = "macos")` early-return in
   `reconnect_saved_hosts` (`app.rs:2308-2311`), which bypasses the
   policy. Both platforms' policies already run as unit tests on
   either OS.
2. **Bundle + sign.** `bundle-iced.sh` adds `roost-session` at
   `Contents/MacOS/`, with its **own explicit `codesign_or_die` step
   before the outer app sign** — the outer sign is non-`--deep`, so a
   nested Mach-O ships improperly sealed unless signed individually
   (the exact `roostctl` pattern at `bundle-iced.sh:179`). Verifiable
   pre-tag with the local notarize dry-run. `release.yml` and
   `make-dmg.sh` need nothing — the DMG packages the `.app` whole.
3. **CI — the third gate.** The session lanes are Linux-only twice
   over today (`runs-on` plus `e2e-mac`'s
   `not session_daemon and not host_client` deselect). Floor for this
   slice: a **macOS `session-e2e` cell** — that lane drives no UI and
   needs no display, so it is CI config (the libclang step swaps for
   Xcode's clang). Stretch: `test_host_client.py` on the macOS runner
   (full UI + daemon; interacts with the runner's single-instance
   constraints) — decide in the plan, and if deferred, file it rather
   than dropping it.
4. **Docs.** Mac section in the guide; a launchd-agent recipe as the
   sibling of the Linux `enable-linger` reboot-survival recipe
   (doc-only — supervision units stay deferred, see Open questions).

**Cut from this slice:** no darwin bootstrap, no standalone darwin
release artifact, no changes to `check_os`'s deliberate Darwin refusal
— all HS-4c. The Swift Roost.app stays untouched (direction #5).

**What actually shipped (plan 041, PR #389 — four commits).** The four
items above landed as enumerated, with three corrections worth carrying
forward:

- **The un-gate needed a helper, not a deletion.** Removing
  `reconnect_saved_hosts`' redundant `cfg!` outright would have left a
  hypothetical gated build able to auto-connect (`IfPresent`) a
  localhost host that `verbs()` withholds Disconnect and Stop for. It
  now reads the same policy through a pure `reconnect_mode` helper. The
  fallback value is still only *partly* coherent — `spawn_gate`
  downgrades rather than refuses, and the sidebar ↻ and
  `roostctl host connect` read no policy — which is pre-existing and
  deliberate (dialing a running session was never gated, D11), but
  whoever ever ships `localhost_surface: false` has to gate those two
  routes too. Recorded at `the_gate_covers_both_directions`.
- **Two sibling directories, not one.** The bundle serves two callers
  of the discovery ladder: the UI's own exe sits in `Contents/MacOS/`
  and the bundled `roostctl` in `Contents/Resources/bin/`. The real
  Mach-O lands in the former (D6's pin) with a *relative* symlink in
  the latter; the ladder's `"next to roostctl"` copy became
  `"next to this program"` accordingly.
- **Signature assertions have to key on the hardened-runtime flag.** On
  Apple Silicon the linker ad-hoc-signs every arm64 Mach-O, so a raw
  `cargo build` artifact already passes `codesign --verify --strict`
  and yields a zero-byte entitlements dump — neither distinguishes "we
  signed it" from "we never touched it". Only `--options runtime` sets
  the `runtime` flag, so that is what the bundle self-check, the outer
  sign's gate, and CI's assert step all test. The nested daemon ships
  with an **empty entitlements dict** (a static Rust binary needs no
  hole punched through the hardened runtime). Verified end to end: a
  local notarize dry-run came back **Accepted** with the symlink intact
  in a mounted DMG.

Verification beyond CI: `make e2e-session` passes on macOS (47 tests,
first run of that lane on the platform), and the live criterion on a
real Mac covered spawn-from-bundle → marker → quit → survive →
relaunch → auto-reconnect → Stop. The `session-e2e (macos-latest)` CI
leg is green.

**Left open by this slice:**
[#390](https://github.com/charliek/roost/issues/390) the macOS
`test_host_client.py` lane (the stretch item, deferred — it is a new
lane class, not a cell swap: no macOS iced-UI lane exists in CI at
all);
[#391](https://github.com/charliek/roost/issues/391) the Swift
`bundle.sh` keeps the nested-signature hole this slice closed for the
iced bundle (plan-pinned as untouched);
[#392](https://github.com/charliek/roost/issues/392) with the daemon
binary unreachable the sidebar band reports an IPC io error instead of
the ladder's actionable three-rung message — newly reachable on macOS,
and not diagnosable without [#382](https://github.com/charliek/roost/issues/382)'s
connection-state op.

#### HS-4c — Mac as an SSH host

Connect *to* a Mac the way HS-3 connects to a Linux box. Everything
HS-4b ships, plus the bootstrap grows an OS axis: `check_os` accepts
Darwin; the platform map and `asset_name` gain an OS dimension
(darwin-arm64 only, matching the DMG); the candidate ladder gets
Darwin paths validated against macOS `sshd`'s narrow non-interactive
`PATH`; and a **cross-OS source rule** — streaming this client's own
sibling binary is same-OS-only, so a Linux client bootstrapping a Mac
(or vice versa) falls to the checksum-verified download or a local
override. Ships the standalone `roost-session-<v>-darwin-arm64`
artifact + checksum, with a signing decision for a bare Mach-O
streamed over SSH. **Main cost and why it trails HS-4b:** validation
is manual-only — there is no macOS analog of the shed VM — and the
artifact matrix only fully proves itself at tag time, so it lands
after HS-4b has proven the Mac binary in the wild.

#### HS-4d — Multi-client: viewports vs. takeover integrity

Two alternatives to weigh together, not a sequence: **independent
per-client viewports** (replacing takeover outright) versus
[#381](https://github.com/charliek/roost/issues/381)'s narrower fix —
an atomic conditional-connect on the wire ("take over only if the
lease is still X", a `session_protocol` bump) closing the best-effort
guard and the third-client-tombstone hole plan 040 recorded. A
viewports design that removes takeover closes #381 by construction.
#381 sits in the pre-release list, so **this decision is on the
release path** even though the viewports plan itself is not: before
the first release, either #381's fix ships or the viewports direction
is chosen and #381 is re-tagged as subsumed.

#### HS-4e — Porcelain, sequenced by usage (unscheduled)

"Move tab to host"; named sessions per host (`workbox:agents`);
second-window-as-second-client if the multi-window question becomes
live; the first real per-host setting when something needs one
(#388's pinned convention); a `roostctl session install-unit`-style
supervision helper if the launchd/systemd doc recipes prove popular.
None of these starts without a usage signal.

#### Sequencing toward the first release (proposed 2026-09-01)

**HS-4b ✅ + the pre-release follow-ups (#381 — or its HS-4d
decision — #382, #383, #384, plus #392 and #393 added 2026-09-02) →
first release → HS-4c → HS-4d's chosen path → HS-4e by usage.**

**Order within the pre list (recommended 2026-09-02):** #382 first — it
is small, and it is the enabler the rest lean on: it makes #392's band
copy assertable, it replaces #384's log-scraping with real state, and it
is what turns "read the PNG" into a test. Then #393+#384 together, since
a live harness and the ability to place a build on a remote are one
capability. #392 rides along once #382 exists. #383 is testing-debt that
pays for itself but blocks nothing. #381 is the odd one out — it is a
*design* decision (atomic conditional-connect vs. adopting HS-4d's
viewports, which closes it by construction), not an implementation task,
and wants a conversation before a plan.

**The pre list's own items — closed by plan 042 (swept 2026-09-03).**
Four of the five items above have shipped: [#382](https://github.com/charliek/roost/issues/382)
(`host.status` — a new op reporting every saved host's connection state,
retry schedule and rollup, plus `roostctl host status`; `test_host_ssh.py`
now asserts on that state instead of scraping `tracing` log lines for
substrings), [#392](https://github.com/charliek/roost/issues/392) (a
localhost session whose `roost-session` cannot be started now settles
once and names why, instead of re-burying the spawn ladder's message
under `io error: No such file` on every unbounded retry forever; the new
`test_host_local_missing_daemon.py` E2E lane asserts it), [#393](https://github.com/charliek/roost/issues/393)
(`tools/session/dev-session.sh` builds a matching `roost-session` on the
target's architecture in a shed and installs it over the product's own
bootstrap; `ROOST_SESSION_INSTALL_BIN` now refuses a provably wrong-arch
binary before it streams, naming both arches), and [#384](https://github.com/charliek/roost/issues/384)
(the live SSH-reconnect harness is in-repo now, at `tools/session/live/`,
ported from `tracing`-log needles to assert on `host.status`; it is
still not a CI lane — see [#395](https://github.com/charliek/roost/issues/395)
below). Only [#381](https://github.com/charliek/roost/issues/381) and
[#383](https://github.com/charliek/roost/issues/383) remain open on the
pre list.

One thing the #384 port surfaced and left as a live product wrinkle: an
ssh failure's classified family never reaches a published `reason` on a
retryable rung — the armed-rung line overwrites it in the same
`host.status` update — so it surfaces only in the give-up copy. The
live harness's L5 works around it by asserting the refusal as an OS
fact rather than reading it back as a wire fact.

**Release-blocking work from outside this roadmap — closed by plan 043
(swept 2026-09-02).** This list stayed scoped to items host-session work
surfaced, so these were named rather than absorbed, and every one of
them has now shipped: [#355](https://github.com/charliek/roost/issues/355)
(macOS notification authorization was checked once at launch and cached
forever, both UIs) and [#369](https://github.com/charliek/roost/issues/369)
(`tab.focus` over IPC never cleared the sidebar badge) both land directly
on the agent-notification routing this product is differentiated by, and
HS-4b amplified both — host tab focus routes through that same op, and a
Mac host client keeps sessions alive long enough for a mid-session
permission change to matter. [#266](https://github.com/charliek/roost/issues/266)
was a cheap iced-only correctness bug that Mac already had fixed —
exactly the drift the north star's parity rule exists to catch.
[#356](https://github.com/charliek/roost/issues/356) was release
machinery: the mac job never proved `SPARKLE_ED_PRIVATE_KEY` matched the
bundled `SUPublicEDKey`, while the iced job already did the real
derive-and-compare — a mechanical port, not a design question. Riding
alongside as a mirror, not a new design,
[#391](https://github.com/charliek/roost/issues/391) — the
tolerated-signing-failure hole `bundle-iced.sh` already guarded against
but `bundle.sh` left open when this slice closed it for the iced bundle
only (see "Left open by this slice" above) — is closed too.

[#351](https://github.com/charliek/roost/issues/351), the same
differentiator failing on the default Linux desktop stack (a
notification click switches tab but never raises the window, because
winit's `focus_window` is a documented Wayland no-op and the
`xdg-activation` token we already receive goes unused), is **not** part
of that close-out: plan 043 weighed it during planning and cut it, and
it is now the **top post-release item**. The reason in one line: raising
a Wayland window needs a compositor-minted `xdg-activation` token from a *real*
notification daemon's *real* click, which none of the headless shed
tiers can produce and the roosttest mock server cannot mint, so the only
proof is a person clicking a banner on a real desktop — the wrong shape
for a plan whose spine is "provable before strangers use it." Worth
being precise about for release notes: banner-click-raises-window
already works on macOS today (`UNUserNotificationCenter`'s default
action raises the app, on both the Swift app and Roost-Iced) — #351 is
a **Wayland-only** gap, not a cross-platform one.

**The ghostty adoption items: none of them ship this release, and none
of them needs a pin bump.** The pin already moved to main tip
(`f2d5758f`, zig 0.16, 2026-08-27), which is what filed #364-#367 in the
first place — so the "943 commits behind" note above is stale, and every
one of those issues consumes C API that is **already vendored**. Adopting
is decoupled from bumping; no further bump is planned or wanted here.

Verdict per item, decided 2026-09-02:

- **#365 (`ghostty_terminal_paste`) — defer.** Tempting because paste
  safety sounds release-shaped, but the dangerous half is already done:
  `roost-ui-model::bracketed_paste::wrap` neutralizes embedded
  `ESC[200~`/`ESC[201~` markers (the `ESC[201~rm -rf /` clipboard
  attack), matching on the output tail so dropped markers cannot splice
  into a fresh one, and it is mirrored byte-for-byte in
  `mac/Sources/Roost/BracketedPaste.swift` under shared test vectors.
  What upstream adds beyond that is the *unsafe-paste confirmation
  dialog* — real footgun protection, but UX rather than a
  vulnerability, and adopting it means re-routing paste through a new
  FFI callback API on **both** UIs plus new modal UX on both. Replacing
  a cross-platform-tested input path and adding two modals under
  release pressure is a bad trade for a guard we already have the
  security-critical part of.
- **#364 (native notification + progress OSC) — defer the
  implementation.** It touches the differentiator, which makes it the
  one worth wanting, but it is blocked on a genuine constraint:
  `OPT_USERDATA` is a single shared slot already fully consumed by
  `write_pty` (`crates/roost-vt/src/terminal.rs:495-518`, exclusivity
  documented there), so it needs a multiplexed-dispatcher redesign of
  safety-critical FFI. A design-only pass is welcome as its own
  exercise; the refactor is not release work.
- **#366 (idle scrollback compression) — defer**, and measure before
  implementing: the issue's own framing asks for a profile first, and
  there is no observed memory problem to point at.
- **#367 (libghostty-rs) — nothing actionable**, blocked on an upstream
  release past 0.2.1.

*Kept for whenever a bump does happen:* the timing is asymmetric now
that HS-4b made a long-lived daemon real on both platforms. Bumping
while daemon and client ship in one artifact is lockstep-safe; bumping
after users have sessions running means each one must restart its daemon
or its attaches fail the exact-match gate — cleanly, with
`build-mismatch`, so the wire fails safe rather than corrupting a
snapshot, but it still fails and would need release-note guidance.

HS-4b shipped 2026-09-01 (plan 041, PR #389),
so **what stands between here and the first release is the pre-release
follow-up list, nothing else on the milestone track.** Two of those
four got sharper evidence from HS-4b: #382 (no connection-state op) is
now also what blocks diagnosing #392 and what forces band assertions to
be read as pixels, and #390 joins the testing-gap family though it is
not itself release-blocking. **Updated 2026-09-03:** plan 042 closed
#382, #384, #392 and #393, so the pre list is now #381 and #383 alone —
see the sweep paragraph above. Rationale for the order: HS-4b was small,
locally verifiable pre-tag, and rounds out the platform story the
release tells (both platforms persist locally, both connect to Linux
hosts);
HS-4c stays post-release because its risk is concentrated exactly
where a first release is weakest — tag-time artifact surface and
manual-only validation. The release itself also carries the standing
live ritual: the shed L1–L4 reconnect checklist and the Mac→shed
criterion, re-run against the release build.

### HS-5 — Local default flip (decision point, not scheduled)

Launch iced as a client of a local session always, with
`--ephemeral` as the escape hatch. Explicitly a product decision to
be made after living with HS-2/3; the HS-0 seams exist so it is a
default-backend change, not a rewrite.

---

## Tracked follow-ups (pre / post initial release)

Everything below is a **filed GitHub issue**, so this list stays a map
rather than a second backlog. The split is what has to be true before
the first public release versus what is a real improvement we are
deliberately not blocking on. Correctness and the testing that protects
it are pre; maintainability, deferred product decisions and contained
workarounds are post.

Only items surfaced by shipped host-session work are categorised so
far — the rest of the issue tracker predates this split and is
uncategorised, not implicitly "post".

### Pre initial release

**Release decision (2026-09-03): the pre list is closed for v0.0.19.**
With #382, #384, #392 and #393 shipped by plan 042 (PR #396) and
043's notification/release items by PR #394, the first release carrying
host sessions goes out from here; the two items still open below —
#381 and #383 — move to **post-v0.0.19 by decision**, not by
completion. Why it is safe: nothing in 042/043 changed a persisted
format (`state.json`'s registry is 041's), `session_protocol` is still
`2`, and #381's likely outcome — a protocol bump — is already the path
the identity gate + the bootstrap's staged reinstall exercise (a stale
daemon reads `NeedsRestart`, the consent card reinstalls from the
release asset). Why it is right: #381 is a design conversation (atomic
conditional-connect vs. HS-4d viewports) whose regression coverage —
`host.status`, the lanes, the live harness — now exists *before* the
wire changes; #383 is a testing investment that pairs with #386 and
should land before #381's implementation, not before a release.
Post-release order of work (updated 2026-09-03 after hand-testing the
candidate): **#398** first — drag-reorder of a remote host's projects
and tabs, the parity gap a user hits daily — then **#397** (the
`ROOST_STATE_DIR` seam collision); then **#387** (a far side that is
merely restarting should not need a manual ↻ — the one "honest but
unhelpful" first-release behavior) and the `reason`-overwrite wrinkle
(an ssh family unreadable on the wire while a rung is armed), the two
cheap correctness follow-ups; then #383 + #386 together; then the #381
conversation; HS-4c stays after that; #351 remains the top non-host
post-release item.

- **[#381](https://github.com/charliek/roost/issues/381) Session takeover is best-effort; needs an atomic
  conditional-connect on the wire.** Reconnect *is* takeover
  (`session.connect { takeover: true }`), and the guard in front of it
  can miss a real takeover, while the server keeps only the
  most-recently-displaced tombstone — so a third client's takeover
  erases the record of the second's. HS-4 slice 1 shipped **parity with
  localhost's best-effort guard, explicitly not "cannot steal"** (plan
  040 §3.7). A real guarantee is a `session_protocol` change and its own
  plan. Multi-client is the point of host sessions, so this is a
  correctness issue with a wrong outcome in both directions.
- **[#382](https://github.com/charliek/roost/issues/382) No IPC op reports host connection state or the retry
  schedule. — Shipped, plan 042.** The functional lane asserted on
  `tracing` message substrings, and the sidebar band copy
  (`reconnecting in 8s (3/10)`) was reachable only by rendering the
  window and reading the PNG. Both were one refactor away from passing
  while the feature was broken. See the closure paragraph above.
- **[#383](https://github.com/charliek/roost/issues/383) `App` is not constructible in a unit test.** `App::bootstrap`
  measures fonts, builds a runtime, hydrates the workspace and binds the
  socket, so every `App`-level guard gets e2e coverage or none — and
  that is exactly where plan 040's late bugs lived.
- **[#384](https://github.com/charliek/roost/issues/384) The live SSH-reconnect harness lives outside the repo. —
  Shipped, plan 042.** Only a real `sshd` exercises `ControlPersist`, a
  black-holed route hitting `ConnectTimeout`, and a daemonised master
  outliving the app. It found real bugs, and it encoded three traps that
  each made a lane report PASS while proving nothing. See the closure
  paragraph above; the CI-lane gap it leaves is #395.
- **[#393](https://github.com/charliek/roost/issues/393) No easy way to
  get a matching dev `roost-session` onto a remote host. — Shipped,
  plan 042.** Scoped with #384, because they are two halves of one
  capability: a live harness needs a way to *place* the build it tests.
  The install half already existed (`ROOST_SESSION_INSTALL_BIN`,
  ungated, staged verify-before-commit) and so did the hard part of
  cross-building from a Mac (`tools/shed/build-in-shed.sh` already
  redirects `CARGO_TARGET_DIR` and bind-mounts the ghostty dirs) — what
  was missing was that the shed builder did not build `roost-session`,
  nothing carried the artifact back out, and the arch axis was a silent
  trap. See the closure paragraph above.
- **[#392](https://github.com/charliek/roost/issues/392) A missing
  daemon binary reports an unusable error. — Shipped, plan 042.** The
  spawn ladder produced the right three-rung message, then localhost's
  own retry ladder buried it: `connect_loop` sets `mode = Dial` after
  the first attempt (`host_conn/task.rs:309-310`), `ensure_socket`
  returned `Ok` for `Dial` without probing, and each retry's generic
  dial failure overwrote the band with `io error: No such file`.
  Localhost's ladder was **unbounded** — the attempt budget is
  `SSH_ATTEMPT_BUDGET`, SSH-only — so a broken install retried forever,
  re-burying the message each time. The verdict was already written
  down for the SSH side: `retryable()`'s table gives `NotFound` a "no —
  nothing to exec, and a retry cannot install it"
  (`host_conn/reconnect.rs:102`); the localhost spawn failure was the
  same class and arrived as `DropInput::Session(None)`, which retried.
  See the closure paragraph above.

### Post initial release

- **[#398](https://github.com/charliek/roost/issues/398) Drag-reorder
  projects and tabs within a remote host's section.** Found
  hand-testing the v0.0.19 candidate: the LOCAL section reorders both,
  a host section neither — a deliberate gap (host sections sit outside
  the local reorder strip, and a host project's tab strip is disabled
  because reordering its tabs is an op-queue mutation, not a local
  reorder). The fix routes the drop to the session's own
  `project.reorder` / `tab.reorder` over the host connection and lets
  the mirror re-render from the session's events, with a
  `test_host_client` case. **First item of the next release** — the
  parity gap people hit daily.
- **[#397](https://github.com/charliek/roost/issues/397) A spawned
  `localhost` daemon inherits `ROOST_STATE_DIR`** and then refuses the
  UI's own state lock. Real users never set the seam, and W2 reports it
  honestly (`roost-session failed to start` with the daemon's verdict in
  `detail`), but a UI launched with an isolated state dir can never
  spawn its own daemon. Hand the daemon a derived dir when the seam is
  set. Second, right behind #398.
- **[#385](https://github.com/charliek/roost/issues/385) No seam to inject a tunnel failure**, so a session-drop
  family is unreachable from a unit test and the fire-time re-check
  needs a `#[cfg(test)]` enum arm. Contained and documented.
- **[#386](https://github.com/charliek/roost/issues/386) `HostConnSet`'s six parallel `HashMap`s want one
  `HostEntry`.** Deferred on purpose so the consolidation lands *after*
  the call sites exist; a lifecycle-clarity investment, not growth.
- **[#387](https://github.com/charliek/roost/issues/387) `NoSession` settles immediately**, so a far side that is
  merely restarting costs a manual ↻. Consistent with localhost,
  documented, and mitigated by running `roost-session` as a lingering
  `systemd --user` unit.
- **[#388](https://github.com/charliek/roost/issues/388) Per-host settings: convention pinned, no toggle ships.** A
  settling ladder needs no off-switch; filed so the convention
  (additive `#[serde(default)]` field, surfaced as a palette verb per
  D11, never a dialog) survives.
- **[#395](https://github.com/charliek/roost/issues/395) The live
  SSH-reconnect harness (`tools/session/live/`, #384) has no
  `workflow_dispatch` CI lane.** It runs against a real `sshd` and takes
  ~8 minutes (L2 alone), so it stays a person's `roost-dev` command, not
  a required gate; a manually-triggered lane would let it run on demand
  without holding up every PR.

**Closed by shipped work:** [#379](https://github.com/charliek/roost/issues/379) (six unbounded stderr drains in
`ssh.rs`) is fixed by plan 040 C1 — bounded drains plus a truncation
flag, so a failure classified from evidence we could not fully read is
never retried.

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
- Named sessions per host (`workbox:agents`) → HS-4e, on a usage
  signal.
- `systemd --user` / launchd supervision of `roost-session` (vs
  plain daemon) → plain daemon shipped; the `enable-linger` systemd
  recipe is documented (plan 040 C6), the launchd sibling lands with
  HS-4b's docs, and an `install-unit` helper is HS-4e if the recipes
  prove popular.
- ~~Whether the Mac iced app ships host-sessions before general
  iced-on-Mac parity messaging~~ **Resolved by events**: the Mac iced
  build shipped the client surface with HS-2 (localhost hidden per
  D11's Mac gate) and its SSH payoff with HS-3 slice 1 — host
  sessions ride the experimental Mac app without waiting on parity
  messaging; the Swift app stays untouched.
