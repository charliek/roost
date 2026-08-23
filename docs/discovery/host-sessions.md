# Host sessions (discovery)

Status: **discovery** — not a commitment, not a roadmap slice. Written
2026-08-22 after comparing Roost (iced), Herdr, tmux-style muxers,
upstream libghostty-vt, and Superlogical’s public multiplexer design.
Update this file when a later pass changes the recommendation.

The feature: an opt-in way to connect a **host** (this machine or
another) so projects and tabs on that host survive closing the Roost
window, with native iced chrome. Default Roost stays what it is today.

Companion: expanding agent coverage without blocking this work is
[`agent-watching.md`](agent-watching.md).

---

## Product

Palette → **Connect remote host** → `workbox`, `user@10.0.0.8`, or
`localhost`.

The sidebar gains a Hosts section. Local in-process projects stay as
they are. Selecting a host context-switches the project list and tab
strip to that host:

```text
 Local
   roost
   herdr
 Hosts
   ▼ workbox          ← selected
       api
       web
   localhost
 + Connect host…
```

Tabs and the terminal widget stay native. Closing the app
**disconnects**; it does not stop the host session. Reopening Roost and
connecting that host again shows the same shells.

`localhost` is the same feature without SSH: spawn or attach a session
server on this machine. That is how detach/attach becomes available
locally without changing default behavior for anyone who never opens
the palette.

Two operations must have different names from day one:

| Action | Meaning |
|---|---|
| Disconnect | Unmount from this window. Session keeps running. |
| Stop session | Kill the host-side process. |

Closing the window is Disconnect.

Local ephemeral tabs and host tabs stay separate worlds for the first
cut. Connecting `localhost` does not migrate the in-process project
already open. “Move tab to host” can come later.

---

## Recommendation

Build an **opt-in Roost session server** (`roost-session` / `roostd`)
from `roost-engine`. Iced becomes a client of that engine when a host
is connected. Default iced stays in-process.

This revises vision.md DL-4 (“no daemon”) without reversing the default
architecture: **the daemon is the host-session feature, not the always-on
app.** People who never connect a host never run one.

Do not put Herdr on the far side of Connect host as the long-term
runtime. Do not put tmux/Zellij under the native widget as the persist
layer. Both remain useful as *references* (and a later “attach to an
existing Herdr/tmux session” host type is possible).

### Why this shape

The UX is Roost-shaped: host → projects → tabs. `roost-engine` already
owns workspace, PTY supervision, OSC, agent axes, and `state.json`.
Claude hooks already inject `ROOST_SOCKET` / `ROOST_TAB_ID`; session
PTYs point those at the session socket instead of the UI socket.

A second iced window and a laptop on another machine become the same
problem: another client of the session server. That is why this also
unblocks a later multi-window goal without inventing a new ownership
layer.

### Why not Herdr as the host runtime

Herdr (Apache-2.0 as of 0.8.x) is the existence proof that the workflow
is right: background server, detach/attach, SSH bootstrap, agent
rollups. Using it as the process on the host would ship persist faster
and then leave Roost translating someone else’s model forever.

| Herdr | What it is | Closest Roost thing |
|---|---|---|
| Session | One server process; named sessions are separate sockets | A **host session** |
| Workspace | Sidebar row: one repo / task, owns tabs, agent rollup | A **project** |
| Tab | A *layout* of split panes | Not present today; future splits or a window layout |
| Pane | One PTY that survives detach | A **tab** (one terminal) |

A Herdr workspace is **not** an OS window. Herdr has no window concept;
the TUI is one client of the session. Mapping workspace → Roost window
would duplicate a project tree per window. The useful Herdr lesson for
windows is the session/client split: PTYs live in one place; every
window is a view with its own focused project/tab.

Herdr’s native per-pane stream today is ANSI viewport frames
(`herdr terminal session observe|control`), not raw PTY bytes and not
a libghostty snapshot. A GPU client can consume that, with muxer-in-
the-middle losses. Agent UX would be Herdr’s screen-manifest model
painted in Roost chrome, or ignored.

License is no longer the blocker (Apache-2.0 + MIT). Product coupling
is.

### Why not tmux / Zellij under the widget

Lowest install bar on a remote host, and persist is a solved Unix
problem. The native widget would be painting the muxer’s VT, not the
agent’s. Projects are invented. Images, keyboard protocol, and
truecolor are the classic losses. Ghostty itself can *parse* tmux
control mode (optional `libghostty-vt` build); that is “be a client of
tmux,” which is iTerm2’s model, not the product described here.

---

## Multiple windows (later)

A window should be a **view onto a session**, not a new container
copied from Herdr workspaces.

```text
Host session (owns PTYs)
  projects            ← today’s sidebar
    tabs              ← today’s terminals
  windows             ← presentation only
    window 1: project roost, tab “claude”
    window 2: project herdr, tab “tests”
```

Two windows can look at the same session. Closing a window does not
kill terminals. Closing the last window may still leave the session
running if that host is connected.

A weaker multi-window exists without a session server: several iced
windows in one process, sharing today’s in-process engine. Close all
windows and the PTYs still die. Host sessions make “second window”
and “Roost on another machine” the same attach protocol. Do not wait
on windows to ship localhost persist; do not add a workspace layer to
get windows.

---

## What has to exist on the host

Regardless of chrome, a host process must keep a **parsed screen**
while no window is attached. A dumb byte pipe either grows without
bound or drops agent output the moment Roost closes.

Three different problems get called “scrollback”:

1. **Live history while a window is open.** Roost already does this
   (`roost-vt`, `max_scrollback: 2000`). Not the hard part.
2. **History while no window is attached.** Tmux and Herdr both keep a
   VT on the server, bounded by lines or bytes. A Roost session server
   must copy this.
3. **History after the server itself dies.** Herdr’s optional
   `pane_history` dumps ANSI to disk and replays into a *new* shell.
   Off by default (secrets). Skip for any first cut.

On reattach, the client needs the current screen quickly, then
scrollback. Live PTY bytes follow.

---

## VT dump, snapshots, and the wire

The Roost attach protocol should stay **ours** (JSON IPC or a small
binary framed sibling of `roost-ipc`). The *payload* for “here is the
terminal” should come from libghostty-vt, not from a homemade cell
grid.

### Tmux policy (steal this, not the code)

- The server owns the screen. Always.
- Bound history by lines, not “keep all bytes forever.”
- When no client is attached, keep the last PTY size (or a default).
- Do not replay an unbounded raw log on attach.

### Herdr formatter (working today, lossy)

libghostty-vt can **format** a terminal back into VT/ANSI (also plain
text and HTML). Herdr already uses this for `snapshot_history`, for
terminal-session frames, and for optional disk history. On attach, the
client `vt_write`s that dump, then follows with live bytes.

This is good enough for an MVP that only needs “current screen plus
some history, text/TUI agents.” It is not lossless: graphics, some
modes, and unfinished parser state are the usual gaps.

Roost’s current pin (`third_party/ghostty/build.sh`, `c74f6d5`,
2026-04-25) and Herdr’s vendored libghostty both speak the formatter.
Roost does not wrap it yet; Herdr does (`FormatterFormat::Vt`).

### Ghostty binary snapshot (the better attach primitive)

Upstream libghostty-vt now has a first-class **snapshot** C API
(`include/ghostty/vt/snapshot.h`, example `example/c-vt-snapshot`).
It encodes complete terminal state to a versioned, CRC-protected
binary stream:

- cells, attributes, scrollback pages, both screens
- cursor, modes, colors
- unfinished VT/UTF-8 parser input (**continuation**), if tracking
  was enabled before that input arrived

Layout is deliberately attach-friendly:

1. Active screen + continuation
2. `READY` — terminal is renderable and can take live input
3. History pages, newest to oldest (incremental prepend)
4. `FINISH`

The client can paint as soon as READY arrives, then fill scrollback
without blocking the first frame. The terminal may be rendered,
resized, and fed live PTY bytes between history pages; pages that can
no longer apply safely are skipped.

Ghostty is explicit about what this is **not**:

> This is NOT a full transport-ready format to implement generic
> replay software such as multiplexers, recorders (e.g. asciinema).
> The goal is a documented representation for a terminal state.

Use snapshot bytes as the attach payload inside Roost’s protocol. Do
not treat `GHOSTSNP` as the session wire format. Format version 1 is
still a work in progress and does not yet promise binary
compatibility, so the Roost protocol must version the payload and
tolerate a Ghostty pin bump.

Neither Roost’s current pin nor Herdr’s vendor tree exposes
`ghostty_snapshot_*` yet. Getting it means the already-planned
libghostty pin bump (iced-migration E8: 603+ commits behind
`../libghostty-rs`, and a Zig 0.16 question). Host-session fidelity
is a reason to do that bump; it should not block a formatter-based
MVP.

Continuation tracking must be **on** on the session-server terminal
before attach, or an in-flight CSI/OSC/UTF-8 sequence cannot round-trip
through a snapshot. That is a session-server construction detail, not
a UI detail.

### Ghostty SSH-remote PoC (architecture rhyme, not a dependency)

[Ghostty discussion #11891](https://github.com/ghostty-org/ghostty/discussions/11891)
is a fork: local Ghostty, remote `ghostty-daemon`, custom binary
protocol, persistent sessions, page-diff scrollback, auto-provision
the remote binary, multi-viewer. It is not upstream, not mergeable,
and largely a prototype. It is independent confirmation of the same
shape: native chrome locally, daemon on the host, state sync rather
than nested tmux.

Roost should not wait for Ghostty to ship SSH sessions. Iced +
`roost-session` is that product in this repo.

### libghostty-vt vs Ghostty’s GPU compositor

Roost already does not use Ghostty’s renderer. It uses libghostty-vt
as the parser/screen engine and paints with iced+wgpu (and Core
Graphics on the Swift app). Host sessions do not change that. The
GPU widget stays in iced. The session server keeps a `Terminal` so
detach does not drop output. Attach hydrates the client `Terminal`
from formatter or snapshot, then live bytes.

What you keep vs give up:

| | Default local | Host session, raw bytes + snapshot/formatter |
|---|---|---|
| GPU chrome + cell renderer | yes | yes |
| Client libghostty is the agent’s VT | yes | yes, after attach |
| Output while the window is closed | gone | kept on the host |
| Scrollback across reconnect | n/a | server VT, dumped on attach |
| In-process PTY (no serialization) | yes | no |
| Kitty graphics / pixel mouse | in-process path | only if bytes are raw and both VTs agree |

### Iced, wgpu, tiny-skia — not a remoting protocol

Roost iced is **not** built on Google Skia. `crates/roost-iced` compiles
both iced backends: **wgpu is primary** (Vulkan/Metal/DX12),
**tiny-skia is the software/headless fallback**. tiny-skia is a Rust
CPU rasterizer in the Skia family of APIs; it does not speak Skia’s
`SkPicture` / Chrome compositor display-list formats, and iced has
no protocol for shipping those primitives to another process.

There is no iced remoting layer to adopt. The widget already turns a
libghostty cell grid into renderer-neutral `fill_quad` / `fill_text`
calls. That draw list is local on purpose: cell metrics come from
the *client* font at the *client* DPI, and wgpu vs tiny-skia already
diverge (linear vs sRGB alpha, hairlines). Shipping that list or a
bitmap from `roost-session` would couple the session to one machine’s
fonts, scale factor, and GPU, and it would be far heavier than a
VT snapshot plus live bytes.

The right place to put server smarts is still the **VT**, not the
GUI renderer:

| Put in `roost-session` | Keep in iced |
|---|---|
| PTY, last size, parsed `Terminal`, attach snapshot | Window, sidebar, tab strip, fonts, GPU paint |
| Agent/OSC state, project/tab layout | Pointer, IME overlay, selection hit-testing |
| Input/resize applied to the PTY | Encoding OS keys through libghostty |

A “dumb GPU painter” that only consumes dirty cells is a possible
later payload, but that payload is libghostty render-state (what
`TerminalWidget` already walks), not iced or Skia. Do not plan
around Skia Picture, wgpu command buffers, or VNC-style frames for
this feature.

---

## Architecture to build

Two backends behind one iced chrome. Default launch uses the
in-process engine unchanged. Connect host switches selected
projects/tabs onto a session process.

```text
 iced window
   Hosts / projects / tabs / TerminalWidget (client roost-vt + GPU)
   TabBackend: InProcess | Host(socket)

 InProcess (today’s default)
   roost-engine in the UI process
   PtySupervisor + per-tab Terminal
   roostctl socket = UI socket

 Host (opt-in)
   UDS, or SSH tunnel to a UDS
   roost-session
     roost-engine (headless)
     PtySupervisor
     per-tab server Terminal (libghostty-vt)
     OSC drain + tab.agent_report
     host-side state.json
   roostctl / Claude hooks: ROOST_SOCKET = session socket
```

Control plane stays `roost-ipc` JSON (projects, tabs, events, agent
reports). The new piece is an **attach stream** per tab, Superlogical-
shaped:

1. Pause or briefly quiesce PTY drain.
2. Send attach payload `{ kind: "ghostty-snapshot" | "vt", bytes }`.
3. READY: client `Terminal` can paint, select, scroll, and type.
4. Tee raw PTY bytes to the client. Client libghostty parses in
   parallel with the server Terminal (server stays authoritative).
5. History after READY, newest to oldest, when the payload kind
   supports it (snapshot). Formatter `vt` may only carry the current
   screen; that is acceptable for the first cut.
6. Input and resize go to the server. One writer; a second connect
   takes over until we add independent viewports.

Do not stream iced primitives, cell-diff frames as the hot path, or
tmux/Herdr TUI bytes. The client owns interactive state after READY
(Mitchell: diffs break scrollback and selection).

`roost-session` is a **separate binary** (or `roost session` that
daemonizes). It must survive the UI exiting: `setsid` / double-fork
on Unix, its own single-instance lock, a socket profile distinct from
the UI’s `roostctl` socket (`roost-session` vs `roost` / `roost-iced`).

Saved hosts live in **local** iced state (`hosts` in `state.json` or a
sidecar): `{ id, label, target, last_connected }`. `target` is
`localhost` or an SSH destination. Remote project/tab layout lives on
the host, not in the laptop file.

Agent watching (screen manifests, more adapters) is a separate
discovery note: [`agent-watching.md`](agent-watching.md). Detection
must run where the VT lives (in-process engine today; `roost-session`
for a host). Do not block host sessions on that work.

---

## Execution plan

Ship iteratively. Each phase is usable alone. Do not start SSH, splits,
or flipping the default until localhost persist feels like Roost.

### Phase 0 — Seams, no user-visible change

- Introduce `TabBackend` (or equivalent) so `TerminalTab` takes bytes
  in and writes input/resize out without naming `PtySupervisor`.
- Define attach payload `kind` on the wire even if only `vt` exists.
- Persist an empty `hosts` list. Hosts chrome can exist behind a
  flag or remain unshipped.
- Decide Ghostty pin: if E8 (snapshot API) can land first, Phase 1
  uses `ghostty-snapshot`; otherwise ship `vt` formatter dump and
  keep the kind field.

Acceptance: default iced is indistinguishable from today.

### Phase 1 — Localhost persist (first launch of the feature)

This is the MVP.

- Palette: **Connect remote host** → `localhost` (and a named alias
  like “This machine”).
- Spawn `roost-session` if needed; connect over the session UDS.
- Hosts section in the sidebar; selecting the host shows that
  session’s projects and tabs.
- Attach each visible tab: payload + live bytes into existing
  `TerminalWidget`.
- Quit iced = disconnect. Session keeps PTYs. Reopen + connect =
  same shells, native chrome.
- Explicit **Stop session**.
- Second iced connect to the same localhost session: **takeover**.
- Hooks: session PTYs get `ROOST_SOCKET` pointing at the session.

Not in Phase 1: SSH, splits, independent viewports, disk scrollback,
migrating in-process tabs into the session, auto-start of
`roost-session` on every launch.

Acceptance: close Roost, agents keep running, reopen, same tabs,
still looks like Roost.

### Phase 2 — SSH / remote host

Same client, new transport.

- Palette accepts `workbox`, `user@host`, `ssh://…` the way
  `herdr --remote` does (OpenSSH config, ControlMaster).
- Bootstrap: if `roost-session` is missing on the far side, copy or
  install a matching binary (Herdr’s remote attach is the checklist:
  platform detect, PATH, `~/.local/bin`, keepalive).
- Forward the session UDS (or stdio-mux the same protocol).
- Hosts row is the SSH target. Disconnect vs stop still apply
  (stop = remote `roost-session` exits).

Acceptance: laptop B opens Roost, Connect `workbox`, native tabs
control PTYs that have been running on A.

### Phase 3 — Make the primitive feel inevitable

Only after Phase 2 is boring:

- Independent viewports (second window or second laptop scrolls
  without moving the other). Superlogical’s multi-client rule.
- Auto-reconnect saved localhost (and optionally SSH hosts) on
  launch.
- Optional “Move tab to host” from ephemeral Local into a session.
- Splits, if we want them, as a Roost layout on the session, not a
  nested muxer.

### Phase 4 — Flip the default (later, explicit)

Today’s default stays for the **first ship** of host sessions. After
localhost + SSH are trusted, consider:

- Launch iced as a client of a local `roost-session` always.
- “Local” in the sidebar is that host. Quit does not kill terminals.
- Keep `--ephemeral` / in-process as an escape hatch (`herdr
  --no-session` analogue).

Do not silently flip this. It changes the meaning of quitting the
app. The Phase 0/1 seams exist so the flip is a default-backend
change, not a rewrite.

### Out of scope for this track

Herdr as the host runtime. tmux/Zellij under the widget. Web or iOS
clients. Superlogical’s “multiplexer for all work.” Screen-manifest
agent detection (see [`agent-watching.md`](agent-watching.md)).
Matching “the window never closed” pixel-for-pixel.

### Complexity

Not rebuild Herdr. Not a weekend. Phase 1 is the real slice
(headless lifecycle + attach stream). Phase 2 is SSH bootstrap on
the same protocol. Phase 4 is a product decision, not an
architecture one.

### Before coding (not blocking this note)

Name these so they do not surprise a later implementation PR. They
do not need answers in this discovery file.

- **Iced-only client at first.** Swift remains the Mac daily driver
  (M6). Phase 1 persist will not exist in `Roost.app` until iced-on-
  Mac is the client, or Swift grows a host backend. GTK should not
  get one. Say that in the first implementation plan.
- **Two sockets.** UI `roostctl` socket vs session socket. PTY
  children must see the session. A `roostctl` typed in iTerm/Ghostty
  outside Roost may still hit the UI. Target selection needs a rule.
- **Packaging.** The `.deb` and (later) Mac bundle must ship
  `roost-session` beside the UI, with a version the SSH bootstrap
  can match.
- **Lifecycle on the host.** Logout, `systemd --user`, launchd,
  iced crash leaving a live session, stale sockets. Herdr already
  paid for these; copy the checklist, not the code.
- **Headless PTY environment.** Last size or a default; `TERM`;
  color/theme queries with no window; OSC 52 clipboard (whose
  machine?); image paste. Phase 1 can be “last size, local
  clipboard.” Phase 2 must decide the remote clipboard story.
- **Version skew** between iced and `roost-session` (local upgrade,
  or laptop vs workbox). Attach `kind` and IPC identity exist so
  this is a handshake, not a crash.
- **Do not edit `vision.md` or `AGENT_ROADMAP.md` in the discovery
  PR.** Those change when an implementation plan commits to a DL
  revision.

---

## Superlogical

Mitchell Hashimoto ([@mitchellh](https://x.com/mitchellh))’s company
([superlogical.com](https://www.superlogical.com/), announced 2026-07-29)
is building a commercial terminal multiplexer on the same MIT
libghostty Ghostty donated to a non-profit. The terminal product is
the first slice of a larger “multiplexer for all work” (agents, CI,
sandboxes, production). Ghostty itself stays non-profit; Superlogical
consumes libghostty like any other app and says it will upstream
shared terminal work.

This is not a codebase we can vendor. It is the most detailed public
statement of the architecture we recommended for host sessions.

### Their attach protocol (from Mitchell’s 2026-07-30 architecture video)

Traditional muxers sit *between* a GPU terminal and the PTY:

```text
Ghostty/Kitty  →  tmux/Zellij (a second, slower VT)  →  PTY
```

That inner VT is often ~100× slower, does not speak graphics, and
breaks native scrollback and selection. Putting a fast emulator in
front of tmux does not fix it; the muxer still owns the screen.

Superlogical’s alternative, in their words:

1. Server owns the PTY and an authoritative libghostty `Terminal`.
2. On attach, **pause PTY processing**, send a custom **binary
   snapshot** of just enough state to render (visible content, size,
   cursor, mouse), then a **ready frame**.
3. At READY the client can type, select, and scroll. It does not wait
   for full history.
4. After READY, **tee raw PTY bytes** to every client, SSH-style.
   Each client is a compliant libghostty emulator. Parsing is
   duplicated in parallel so the local display does not wait on the
   server to emit a render stream.
5. Scrollback follows in the background, **newest to oldest**.
6. Each client owns its **own viewport and selection**. One person
   scrolling does not scroll everyone (tmux’s classic shared-viewport
   bug).
7. Input is serialized on the server (one writer, many readers). A
   misbehaving client only corrupts its own view.
8. If the client desyncs, redo the attach. Resize is currently
   last-connection-wins, to be refined.

Mitchell, on why not live cell diffs: “It breaks scrollback and
selection and so on. It’s easier when the client owns the state
rather than raw diffs.”

That is the Ghostty snapshot API (`READY`, then history pages) plus
live byte tee, described as a product. It is also the opposite of
remoting iced/wgpu/Skia primitives.

### Their chrome (from later UI previews)

- Native per platform: Swift/AppKit on Apple, WebGL + libghostty on
  the web, iOS later. Not one cross-platform UI kit.
- Sessions in the chrome (icon beside tabs). Restart restores.
  Another computer or a phone can join.
- Tab peek / mission-control of live Metal-rendered previews,
  including splits, without resizing the live terminal (no reflow).
- Designed from the start for local *and* remote sessions, long-lived
  processes, agent harnesses, rich TUIs, and ephemeral processes.
- “The best multiplexer is the one that makes you forget it exists.”
  Everything on screen is already a remote terminal.

### What to steal, what not to chase

Steal: pause → binary snapshot → READY → tee bytes; client-owned
viewport; newest-to-oldest history; smart libghostty client; native
chrome rather than a TUI muxer inside a GPU terminal.

Do not wait for Superlogical to ship, and do not expand Roost into
“multiplexer for all work.” Their second and third plan items
(composability, production operability) and a web/iOS client are
out of Roost’s current product. Roost’s host-session MVP is the
same *terminal* architecture, scoped to iced + an opt-in
`roost-session`, with agent chrome Roost already has.

Related systems, if we need more examples later: WezTerm mux (GUI
client of a mux server), mosh (local state, predictive echo),
iTerm2 tmux integration (native tabs over a weaker inner VT),
VibeTunnel (web control of local terminals), Herdr (TUI client of a
server-owned VT), the Ghostty SSH-remote fork (#11891).

---

## Open questions

- Auto-reconnect saved hosts on launch, or wait for an explicit
  Connect? Localhost persist wants auto-reconnect; SSH may not.
- One session per host vs named sessions on a host (`workbox:agents`).
  Start with one default session per host.
- Does the empty Hosts section hide until the first connect, so the
  default sidebar is pixel-identical to today?
- Iced-only for the first client, with Swift remaining in-process
  until M6, or block on a single UI?
- Formatter MVP now vs waiting on E8 for snapshot. Recommendation:
  design the attach op to carry an opaque payload + kind
  (`vt` | `ghostty-snapshot`), implement `vt` first if the pin
  lags.
- Should `roost-session` be a separate binary or `roost --session`
  on the iced binary? Separate binary is easier to leave running
  after the UI exits.
- Independent viewports for a second attach (Superlogical) vs
  takeover (our MVP). Takeover is simpler; independent scroll is
  the multi-window/multi-laptop behavior we will want later.

---

## Vision.md non-goals this would change

If this ships, these recorded non-goals become host-session
exceptions rather than global law:

- **No daemon** — default still none; host sessions are opt-in.
- **No remote / network IPC** — still no public network protocol;
  SSH is a tunnel to a Unix socket on the host.
- **No rendered output over the wire** — still true for the hot
  path (PTY bytes + a snapshot/formatter blob on attach, not cell
  frames every redraw).
- **No multi-window** / **no split-pane** — not required for MVP;
  the session/client split is what makes them tractable later.

Do not edit `docs/development/vision.md` from this discovery file.
Revise those DLs in the implementation plan that actually builds
the feature.

---

## Sources (in-tree)

- Roost: `docs/development/vision.md` (DL-4, DL-7, non-goals),
  `docs/archive/roost.proto` (`StreamPty`),
  `crates/roost-engine` (workspace, PTY broadcast, OSC drain),
  `crates/roost-vt` (in-process Terminal, 2000-line scrollback),
  iced-migration E8 (Ghostty pin).
- Herdr: `docs/next/website/src/content/docs/concepts.mdx`,
  `session-state.mdx`, `persistence-remote.mdx`, `socket-api.mdx`;
  libghostty formatter in `src/ghostty/mod.rs`;
  `herdr terminal session observe|control`.
- Ghostty (`../ghostty`): `include/ghostty/vt/snapshot.h`,
  `include/ghostty/vt/formatter.h`, `example/c-vt-snapshot`,
  `src/terminal/snapshot/main.zig`;
  discussion
  [Native SSH Remote Sessions (#11891)](https://github.com/ghostty-org/ghostty/discussions/11891),
  [binary snapshots (#11998)](https://github.com/ghostty-org/ghostty/discussions/11998).
- Superlogical: [homepage](https://www.superlogical.com/),
  [Mitchell’s announcement](https://mitchellh.com/writing/superlogical).
  Public architecture and UI notes are on X as
  [@mitchellh](https://x.com/mitchellh):
  architecture thread
  [2026-07-30](https://x.com/mitchellh/status/2082936029426892960)
  (“client owns the state rather than raw diffs”),
  UI previews [2026-08-12](https://x.com/mitchellh/status/2087594943749726271)
  and [web+desktop](https://x.com/mitchellh/status/2089718969125265621).
  Cofounder UI notes also from [@almonk](https://x.com/almonk).
