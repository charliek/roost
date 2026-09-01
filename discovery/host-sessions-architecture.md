# Host sessions — architecture

Status: **design, with HS-1 and HS-2 shipped** — HS-1a (plan 035,
PR #372) for `roost-session`'s lifecycle and control plane per §8/§4.1,
HS-1b (plan 036) for the data plane: per-tab server Terminals and the
tab-task pipeline (§3), the binary attach stream (§4.3), the wrapper
wiring §5 specifies, the one-authority reply rule (§6), and
`session.connect` leases + takeover (§4.1). All three of HS-1a's
documented deviations are closed. HS-2 (plan 037, PR #374) shipped the
client side — the iced Hosts UX, `HostConn`, the attach-on-focus data
path — plus the effect envelopes (`tab.effect` for bell + OSC 52
writes) and theme reseed (`session.set_theme`) that were still design
in §6; see
[`docs/development/host-sessions.md`](../docs/development/host-sessions.md)
for how the client actually landed (§9 deviated — noted in place).
SSH transport slice 1 (plan 038, C1-C6) has since shipped too — §10
below is now shipped design, annotated in place; the bootstrap slice
(auto-install/verify a remote `roost-session`) remains design. This
file is the technical design the HS milestone plans implement, and
stays normative for the rest of HS-3; status annotations below mark
what shipped when. Companion docs:
[`host-sessions-roadmap.md`](host-sessions-roadmap.md) (milestones,
pinned direction, the HS-1a/HS-1b split) and
[`host-sessions.md`](host-sessions.md) (rationale). For the shipped
wire's authoritative description (including its deviations from this
design), see
[`docs/reference/ipc.md`](../docs/reference/ipc.md#session-sockets).
It assumes the separately planned Ghostty pin bump
("plan 032": libghostty-vt to main tip with the Zig 0.15→0.16
toolchain move, which is what makes the snapshot C API available)
lands first for anything snapshot-related; that effort tracks in
its own PRs and supersedes issue #333. Claims about snapshot
behavior are verifiable against a Ghostty checkout at the bump's
target SHA (`f2d5758f`).

Terminology: **session** = one `roost-session` process owning
projects/tabs/PTYs on a host. **client** = an iced UI connected to
it. **attach** = the per-tab stream that hydrates and then feeds a
client-side Terminal.

---

## 1. Component overview

```text
 iced client (Linux or Mac)
 ├─ sidebar: Local (in-process, unchanged) + Hosts
 ├─ TerminalWidget           ← unchanged; consumes TerminalSnapshot
 ├─ client roost-vt Terminal ← per tab, hydrated from attach payload
 └─ TabBackend
     ├─ InProcess            ← today's PtySupervisor path, default
     └─ Host(HostConn)       ← everything below
          │
          │ localhost: UDS        HS-3: UDS ↔ ssh -T stdio bridge
          ▼
 roost-session (Linux host)
 ├─ roost-engine (headless): Workspace, PtySupervisor, OSC drain,
 │    state.json, agent axes                          [exists]
 ├─ per-tab server Terminal (roost-vt, feature `server-vt`)  [new]
 ├─ roost-ipc server on the session socket profile    [exists + new]
 │    ├─ control connections (request→response JSON) [exists]
 │    ├─ events connection   (push JSON envelopes)    [exists, provisional/leaseless]
 │    └─ data connections    (binary attach streams)  [new]
 └─ lifecycle: setsid daemon, flock single-instance, stale-socket
      probe, explicit stop                            [exists]
```

One session per host (for now). The client holds, per connected
host: **one control connection** (serial request→response — the
existing `roost-ipc` loop, low-frequency ops only), **one events
connection** (push-only after `events.subscribe`), and **one data
connection per attached tab** (bidirectional binary — server sends
snapshot + tee, client sends input/resize frames, §4.3). All on the
same session socket; a connection's role is fixed by its first
frames.

## 2. Where code lives

| Crate | Change |
|---|---|
| `roost-engine` | Seq numbers on `PtyOutputEvent` (HS-0). New feature `server-vt`: per-tab authoritative Terminal + the tab pipeline below (HS-1). Daemonization helper. No default-build dependency on the ghostty archive. |
| `roost-ipc` | `Session` bundle-profile kind (socket/state/log paths). Same-UID peer check (`SO_PEERCRED`/`getpeereid`) enabled for the session profile. Attach handshake + protocol-version types. Real `events.subscribe` (connection flips to push mode). Binary data-plane framing module. |
| `roost-session` (new crate, bin) | `main`: daemonize → locks → engine headless bootstrap (the `tests/ipc_dispatch.rs` pattern) → serve. Owns nothing the engine doesn't already own; it is packaging + lifecycle + the `server-vt` wiring. |
| `roost-vt` | Snapshot wrapper **(shipped: plan 034, PR #371)** — `Terminal::snapshot()` over `ghostty_snapshot_encode` (Vec-writer trampoline; `_encode_alloc` considered and rejected) + the streaming `SnapshotDecoder` with client-side record-boundary buffering (§5). Reply-drain policy hook (§6) still to come. |
| `roost-iced` | `TabBackend` seam (HS-0). `HostConn` client, new `EngineFeed` pump for host events, Hosts sidebar section, connect/disconnect/stop flows (HS-2). |
| `roost-cli` | Target rules for two sockets; `roostctl session …` verbs (start/stop/status) as thin ops. |

## 3. The per-tab pipeline inside `roost-session`

**Shipped in HS-1b** (plan 036) as
`crates/roost-engine/src/tab_task.rs`, built as specified: the bounded
reader→task channel (32 chunks), seq assignment relocated onto the
task, `vt_write` → OSC scan → reply drain → tee → replay ring in that
order, and the snapshot fence taken on the task between writes.
Constants as pinned: 2000-line scrollback, 1 MiB continuation cap,
2 MiB replay ring, 4 concurrent snapshot encodes session-wide. Two
details the section did not anticipate, both from implementation:
the reply drain goes through a byte-capped (64 KiB) pending buffer
rather than blocking on the PTY writer channel — a child that spews
queries and never reads its input would otherwise deadlock its own
tab — and *both* Exit producers (reader EOF and the reap task's
deadline backstop) route through the task, which drains what is queued
before publishing `Exit`, so `final_seq` is always last byte seq + 1.

Today: `pty_reader_loop` (spawn_blocking, 4 KiB reads) →
`broadcast::Sender<PtyOutputEvent>` (capacity 256) → per-tab drain
task (OSC scan) → UI. A lagged subscriber loses bytes permanently.
That is fine for a UI that owns the only Terminal; it is not fine
when the server Terminal is authoritative.

New pipeline (feature `server-vt`):

```text
 pty_reader_loop
   └─ mpsc (bounded) ───► tab task (owns the server Terminal)
                             1. assign seq (u64, per tab, monotonic)
                             2. vt_write into server Terminal
                             3. OSC scan (existing router)
                             4. drain server-VT reply bytes → PTY input
                             5. broadcast SeqBytes{seq, data} to tee
                                subscribers (attach forwarders)
                             6. append to the tab's bounded replay
                                ring (recent SeqBytes, for attach
                                resume — §4.3)

 (client INPUT/RESIZE frames and snapshot requests enter the same
  tab task — one authority orders output, input, resize, snapshot,
  replies, and exit; the PTY read/write tasks do blocking I/O
  around it)
```

The reader→tab channel is **bounded, deliberately**: when the tab
task falls behind, the blocking reader stalls, the kernel PTY buffer
fills, and the child blocks on write — the classic free terminal
flow control. The alternative (unbounded) turns a firehose child
(`yes`, a runaway build) into unbounded memory growth in a process
designed to run for weeks unattended. So the authoritative Terminal
never *loses* bytes; under extreme output it applies backpressure to
the child instead.

Properties this buys:

- **The authoritative Terminal never misses bytes** — it is fed
  synchronously on its owning task, not via the lossy broadcast.
- **The fence is race-free.** A snapshot is taken *on the tab task*
  (a small command message: `Snapshot(reply_tx)`), between writes.
  The reply is `(seq_at_snapshot, snapshot_bytes)`. Nothing can
  interleave.
- **Terminal-generated replies** (DA, DSR, XTGETTCAP, OSC color
  queries) are answered exactly once, by the server, whether or not
  a client is attached (§6).
- Encode is synchronous on the tab task (the C API requires no
  concurrent mutation); at 2000 lines of scrollback this is
  single-digit milliseconds and acceptable on a per-tab task.

Server Terminal construction (per tab, at spawn): continuation
tracking ON before the first byte (snapshot precondition);
`max_scrollback` matching the UI policy (2000 lines); sized per §7.
Kitty image state does not survive snapshots — documented limitation.

The in-process default path is untouched: without `server-vt` the
reader → broadcast → drain flow stays exactly as today. Seq numbers
(HS-0) are assigned in the reader loop pre-`server-vt` and move to
the tab task when it exists; the field is invisible to current
consumers.

## 4. Wire protocol

### 4.1 Control plane (JSON, existing shape)

Newline-delimited JSON request→response on the session socket, the
existing `roost-ipc` framing and op set. New/changed ops:

- `session.identify` → `{ app_version, session_protocol,
  payload_kinds: ["ghostty-snapshot"], libghostty_build,
  session_id, started_at }` (this doc's generic "protocol_version"
  concept ships on the wire as `session_protocol` — deliberately a
  separate constant from the request/response `protocol_version`;
  ipc.md is normative on field names). The client calls this
  **first** on any
  new host connection; every incompatibility is detected here, on
  stable JSON, before any binary frame exists (Herdr's lesson).
  **Shipped in HS-1a**; **HS-1b populated both fields** —
  `payload_kinds: ["ghostty-snapshot"]` and a real
  `libghostty_build` (`ghostty-<16 hex>+snapshot.v1`, derived from the
  pinned SHA at build time), and bumped `session_protocol` to `2`.
- `session.connect` `{ takeover: true }` → registers the client and
  returns a **lease**: a cryptographically random token that is the
  interactive-authority boundary (a self-declared client id is
  not). The lease is required on `events.subscribe`, `tab.attach`,
  and interactive input; takeover atomically invalidates the old
  lease and closes **all** of the previous client's connections —
  control included — and any op arriving under a stale lease gets a
  clean `taken-over` error. Without this, a displaced or
  half-crashed client could keep injecting input into tabs the new
  client believes it owns. Administrative ops (`roostctl` queries,
  hook-driven `tab.agent_report`, `tab.open`) remain legal without
  a lease — they are same-UID control-plane use, not interactive
  ownership. Response carries the current workspace snapshot
  revision. Required ordering on a new connection set:
  `session.identify` → `session.connect` → `events.subscribe` /
  `tab.attach`; out-of-order ops get a clean error naming the
  missing step. **Shipped in HS-1b** as written, with one resolution:
  the `identify`-before-`connect` half is deliberately *not* enforced
  — `identify` is a stateless read, and the load-bearing ordering is
  lease-before-subscribe/attach, which is. `connect-required` is the
  clean error naming the missing step. Takeover keeps exactly one
  tombstone (the most recently displaced lease answers `taken-over`;
  older ones fall back to `connect-required`) and purges the displaced
  lease's outstanding attach tokens. See
  [ipc.md's `session.connect`](../docs/reference/ipc.md#sessionconnect).
- `session.stop` → graceful shutdown (§8). Distinct from disconnect,
  which is just closing connections. **Shipped in HS-1a**, reap
  report included.
- `tab.attach` `{ tab_id, kinds: [...], cols, rows, cell_px }` →
  validates, returns `{ attach_token, kind }`. The token is
  single-use, short-lived (~60 s TTL), and associated with the
  connected client — possession-based within the same UID, which is
  the UDS threat model, not a cryptographic binding. **Attach
  carries the client geometry**: because takeover guarantees a
  single client, the server resizes the tab (PTY + server VT) to
  the client size *before* encoding the snapshot, so the payload
  arrives at client geometry and no post-READY resize is needed in
  the common case. (This does not violate the detach-doesn't-resize
  rule — attach is exactly when in-process Roost resizes too.)
  **Shipped in HS-1b**, with a pinned validation order and two fields
  this section did not name: the result also carries
  `{server_epoch, tab_generation}`, the resume identity (§4.3), and
  the params carry the client's `libghostty_build` so the exact-match
  gate is checked here rather than left to §4.4's `session.identify`
  advisory.
- `events.subscribe` → implemented for real (below).
- Existing ops (`project.*`, `tab.open/close/list/write/resize/
  set_title/agent_report/dump…`) work unchanged — this is the point
  of the one-op-set architecture. UI-only ops (`palette.*`,
  `window.*`, `app.*`, screenshot) return the existing
  `no UI attached` error on a session socket; `roostctl` target
  selection sends them to the UI socket instead.

Interactive input and resize do **not** ride the control plane. The
control loop is serial request→response (one in-flight request,
response awaited), which is RTT-bound: key repeat, mouse reporting,
and bracketed paste over a 50 ms link would queue behind their own
acks, and any slow op (`tab.dump`) would head-of-line-block
keystrokes. Instead, an attached tab's **data connection is
bidirectional**: the client sends unacknowledged, ordered INPUT and
RESIZE frames (§4.3), bound to the attach. Non-attached input
(e.g. `roostctl` scripting `tab.write`) still works via the control
op — low-frequency administrative use only. Keys are encoded
**client-side** by the client Terminal's `KeyEncoder` (the client
VT tracks modes from READY onward, so kitty-keyboard/
application-mode encoding is correct locally — Superlogical's
"client is a compliant emulator"). HS-1's lane asserts input
latency stays flat while a large `tab.dump` runs on the control
connection.

### 4.2 Events connection (JSON push)

A dedicated connection sends `events.subscribe`; on `ok` the
connection flips to push-only: the server writes the already-spec'd
event envelopes (`docs/reference/ipc.md` — workspace deltas, tab
lifecycle, agent reports) and ignores further reads. Envelopes carry
a workspace revision so the client can detect gaps and re-pull
`tab.list`/`project.list` (resync instead of perfect delivery).
That resync design is also the backpressure story: the server's
per-client event queue is bounded, and a stalled events connection
is closed rather than buffered without bound — dropping envelopes
is safe precisely because reconnect + revision resync recovers.

One engine reality shapes the envelope: a single workspace commit
can publish several events under the **same** revision, so bare
per-envelope revisions cannot detect loss mid-batch. The wire
therefore carries atomic `{ revision, events: [...] }` batches, one
per commit including empty ones, and the events connection uses the
same subscribe-first / snapshot-at-R / discard-≤R fence discipline as
the terminal stream. This is a correctness dependency of HS-2's
sidebar, not a nice-to-have.

**Shipped in HS-1a**, resolving the open "batches or index/count"
question above in favor of atomic batches — see
[ipc.md's `events.subscribe`](../docs/reference/ipc.md#eventssubscribe)
for the wire shape as built: the subscribe ack returns `{revision}`
as the fence, the first batch is exactly `revision + 1`, every commit
(including a quiet one) produces a batch so a skipped number always
means loss, and a bounded per-client queue plus close-don't-thin
backpressure is implemented as specified. HS-1a's one deviation — a
**leaseless** connection — **is closed in HS-1b**: `events.subscribe`
now requires the lease §4.1 gates it behind, which is the breaking wire
change `SESSION_PROTOCOL_VERSION: 2` announces. The stream also gained
the terminal control envelope §8 calls for: every frame is an
`EventBatch` except a final revision-less
`{"event":"session.stopping","data":{"reason":"stop"|"taken-over"}}`,
exempt from the gap check. Consumed by HS-2.

### 4.3 Data plane (binary attach stream)

**Shipped in HS-1b** (plan 036), essentially as specified — frame
types, byte layouts, the 8-byte preamble (`ROOSTDP2`), the 1 MiB
frame cap, the fence discipline, weighted scheduling, and
resume-from-seq all landed as written. Deltas worth knowing: the
handshake carries `server_epoch` + `tab_generation` alongside
`resume_from_seq` (D6's restart-safety fix — see the resume bullet
below); step 3's "queues the rest" is implemented **exactly**, as a
hold — tee records are absorbed but not written until the READY prefix
is fully out, since a client has no terminal to apply them to yet; and
the "coalesces queued `SeqBytes`" of the slow-link section became
per-syscall batching with **one frame per tee record**, not
cross-record coalescing, because a merged frame would have to invent a
seq. The wire's authoritative description is
[ipc.md's Data plane](../docs/reference/ipc.md#data-plane).

One connection per attached tab. First line: JSON handshake
`{ attach: attach_token, protocol_version, resume_from_seq? }`;
server replies one JSON line `{ ok, kind, mode: "snapshot" |
"resume", seq }` — `seq` is the fence value — then the connection
switches to length-prefixed binary frames. The stream is
**bidirectional**: SNAP/PTY/EXIT/ERROR from the server, INPUT/RESIZE
from the client, per the table below. (This handshake shape is
normative; where the roadmap's summary of it differs, this section
wins.)

```text
 preamble: 8-byte magic (guards against SSH banners / stray stdout)
 frame := u32-LE len (payload only, excl. prefix+type) | u8 type | payload
 server → client:
   0x01 SNAP  — next bytes of the encoded snapshot stream
   0x02 PTY   — u64-LE seq | raw PTY bytes (post-fence tee)
   0x03 EXIT  — u64-LE final_seq | i32-LE exit code; always the
                last frame — sent only after all preceding output
                is committed and teed
   0x0F ERROR — JSON diagnostic; connection closes after
 client → server:
   0x11 INPUT  — raw encoded key/paste bytes (unacknowledged,
                 ordered)
   0x12 RESIZE — u16 cols | u16 rows | u16 cell_w_px | u16 cell_h_px
                 (pixel dims are load-bearing: Terminal::resize and
                 mode-2048 size reports need them)
```

Unknown frame type or over-limit length is fatal (ERROR + close);
zero-length payloads are legal only where the type allows. The full
field-level spec (limits per type, error codes) is HS-1 plan work;
these choices are pinned. `seq` is a **per-output-record ordinal**
scoped to `{server_generation, tab_generation}` — the client
expects exactly `S+1`, treats a gap or duplicate as desync, and
generation scoping stops a restarted server's stream from being
silently accepted. Server-side, INPUT/RESIZE frames forward to the
tab task, which orders them with output, snapshot requests, server
VT resize, `TIOCSWINSZ`, and reply drain — one authority for
everything that touches the tab (§3).

Chunk size ≤ 1 MiB. No base64, no 16 MiB JSON-frame ceiling. The
snapshot stream's own record structure (GHOSTSNP: records with
tag/len/CRC, its READY record, history pages, FINISH) rides inside
SNAP frames — Ghostty's READY is the *only* READY in this design;
the transport adds no second marker. Ghostty explicitly designed
the format to be embedded in a larger transport, with live data
multiplexed outside the record sequence — which is exactly what PTY
frames are.

Attach sequence end-to-end:

1. Control: `tab.attach` → token.
2. Data conn: handshake. Server subscribes a forwarder to the tab's
   tee broadcast **first**, then requests `Snapshot` from the tab
   task → `(S, bytes)`. Handshake reply carries `seq: S`.
3. Server sends SNAP frames for the snapshot prefix **through the
   READY record** at full speed; the forwarder discards tee events
   with `seq ≤ S` and queues the rest.
4. Client reaches READY → renderable, typeable, scrollable
   (§5 for how). Client starts applying PTY frames.
5. Server sends the remaining snapshot bytes (history pages,
   newest→oldest, then FINISH) as SNAP frames interleaved with PTY
   frames under **bounded weighted scheduling**: PTY frames lead,
   but after a bounded PTY byte/time burst at least one snapshot
   record goes out — strict PTY priority would let a continuously
   producing command starve FINISH forever. The attach as a whole
   has a duration + byte budget; blowing it is desync. Client feeds
   SNAP bytes to the incremental decoder between `vt_write`s.
6. Desync or decode error → ERROR + close → the client re-attaches
   (fresh snapshot). Re-attach is the universal recovery path; there
   is no repair protocol beyond resume (below).

**Slow links must not thrash.** A naive overflow → full-re-attach
loop livelocks on a link slower than the tab's output (watching a
busy build over SSH is the steady state, not an edge case). Three
mechanisms bound it:

- Each attach forwarder has a named byte budget (pinned in the HS-1
  plan) and coalesces queued `SeqBytes` before writing.
- The tab task keeps a bounded **replay ring** of recent `SeqBytes`
  (§3). On ERROR-close the client reconnects with
  `resume_from_seq = last_applied + 1`; if the ring covers the gap
  the server replies `mode: "resume"` and sends only PTY frames from
  that seq — the client Terminal is already hydrated, no snapshot,
  no history re-send. A ring miss falls back to a full snapshot
  attach.
- Client re-attach uses jittered backoff, and the UI keeps showing
  the previous hydrated Terminal until the replacement reaches
  READY — a retry loop must never blank the tab.

### 4.4 Versioning

`protocol_version` is a single integer covering both planes; any
change bumps it. `payload_kinds` names what the server can emit.
`libghostty_build` (the pinned Ghostty SHA + snapshot format
version) must match exactly for `ghostty-snapshot` — format v1
carries no binary-compatibility promise, so same-build is a hard
gate at `session.identify`, surfaced as "upgrade the host / client"
with the versions shown. The `kind` field keeps a formatter
fallback possible without protocol changes, but it is not built
unless needed.

**The localhost upgrade trap is the common case of this gate, not a
corner.** Every package upgrade after a user opts into localhost
persist produces exactly this mismatch on next launch: the running
session is the old build, the relaunched UI the new one. HS-2 must
ship a first-class flow for it: the UI detects the mismatch at
`session.identify` and offers **"Restart session (ends shells,
keeps layout)"** — layout is in the host's `state.json`, so restart
reproduces tabs as fresh shells in their dirs, same as today's app
relaunch semantics. If living with that proves too painful, the
formatter payload (`kind: "vt"`, version-tolerant) is the designed
escape hatch, and this scenario is its concrete trigger. Over SSH
the same gate means a client upgrade re-runs the remote bootstrap
install — expected behavior, stated in the connect UI.

## 5. Client-side decode (roost-vt wrapper)

**Shipped:** the wrapper this section specifies now exists as
`crates/roost-vt/src/snapshot.rs` (plan 034, PR #371) — envelope +
record-header framing for boundaries only, `decoder_ready`/
`decoder_next` gated on a fully buffered record, and wrapper-enforced
hard caps on *size* only (total bytes, single-record bytes,
continuation bytes); decode *time* is not capped by the wrapper and
stays the consumer's watchdog, as below. What follows is the
specification the wrapper was built to.

**The wiring is shipped in HS-1b too**, though not yet in a UI: the
Rust integration client (`crates/roost-session/tests/
attach_stream_test.rs`) drives the real socket, feeds SNAP bytes into
`SnapshotDecoder`, applies PTY frames through `vt_write` interleaved
with history steps, and asserts the decoded terminal's dump equals the
server's `tab.dump` at the fence — including a fence landing
mid-UTF-8/CSI/OSC/DCS. What is still HS-2 is everything on the UI side
of this section: `TabBackend::Host`, the host pump, and the
resize-withhold state machine.

The Ghostty decoder's `GhosttyReader` is synchronous and
blocking-only (zero-byte read = EOF, never "would block"), the
decoder may retain borrowed input until FINISH, and the client
Terminal lives on the iced main thread. The wrapper therefore never
lets the decoder touch the network, and owns a **stable buffer**
the network pump appends to only at defined points (no reallocation
under the decoder's feet):

- SNAP bytes accumulate in a buffer owned by the tab's host pump.
- The wrapper parses the 10-byte envelope and record headers
  (`tag u16 | len u32 | crc u32 | payload` — documented layout)
  purely to find boundaries; semantic validation stays with the
  decoder, but the wrapper enforces its own hard caps — total
  snapshot bytes, single-record size, continuation bytes — because
  the 1 MiB transport frame limit does not bound an accumulated
  record. (Decode *time* is the consumer's watchdog — the wrapper
  caps size only, matching the shipped code.) EOF before FINISH is
  desync even if no ERROR frame arrived.
- `decoder_ready()` is called only once the buffer contains the
  complete prefix through the READY record; `decoder_next()` only
  when the next full record is buffered. Reads always complete from
  memory; the main thread never blocks.
- After READY the wrapper owns a real `Terminal`: `TerminalWidget`,
  selection, scrollback, and the key/mouse encoders work unchanged.
  The decoder's advisory history-row counts (its
  `HISTORY_ROWS_PRIMARY/ALTERNATE` queries, available at READY)
  size the scrollbar before history lands.
- Because `tab.attach` carries the client geometry and the server
  resizes before encoding (§4.1), the snapshot already matches the
  client and no post-READY resize is needed in the common case. For
  the residual race (a server-side size change between attach and
  FINISH — e.g. takeover landing mid-attach), the client
  **withholds any further resize until FINISH or a 2 s timeout**;
  a resize before FINISH forfeits remaining history (decoder drop
  rule), which the re-attach path recovers. Serializing attach
  against resize server-side was considered and rejected — withhold
  plus accepted-loss is simpler and the loss case is rare once
  geometry rides the attach op.
- Kitty image state does not survive the snapshot: image regions
  render as blank cells after attach (placeholder cells may remain
  as grid content). Documented, user-visible, acceptable.
- Decoder error → drop the attach, re-attach.

## 6. One authority for replies and effects

**The server half is shipped in HS-1b.** The tab task drains the
server Terminal's reply buffer after every `vt_write` *and* every
resize, so DA/DSR/XTGETTCAP/color queries are answered exactly once,
detached — `tab.capture_pty_input` on a session socket reads that
buffer, which is how the exactly-once rule is asserted headlessly, and
the Rust integration client proves the other half by showing the
decoded client Terminal's own reply buffer is non-empty and
verifiably discarded. Effects were the part HS-1b did **not** ship:
`OscAction::Workspace` is applied server-side as specified, but
client-local effects were dropped and debug-logged, with the seam
commented for HS-2. **HS-2 (plan 037) closed that**: `tab.effect`
envelopes now carry bell and OSC 52 clipboard writes to the attached
client, and `session.set_theme` reseeds the server palette from the
client's theme. OSC 52 read stays default-deny (revisit at HS-3).

Both VTs parse the same byte stream, and libghostty generates reply
bytes (DA, DSR, color queries, XTGETTCAP) into a buffer the embedder
drains. Two emulators would answer twice. The rule:

- **Server answers.** The tab task drains the server Terminal's
  reply buffer into the PTY (pipeline step 4). This also makes
  queries work while detached.
- **Client discards.** In `TabBackend::Host`, the client's
  `write_vt` drains the reply buffer to nowhere. (In-process mode
  keeps today's behavior.)
- Client-local effects route to the attached client over the events
  connection: OSC 52 clipboard writes (allow-listed: standard
  clipboard only, single item, text only, bounded size), bell,
  desktop-notification requests. No client attached → dropped and
  logged. The server disables all local side effects (Herdr's
  pattern, one chokepoint).
- **Effects fire exactly once.** Host-tab PTY frames bypass the
  client's OSC action dispatch entirely — the server's scan
  (pipeline step 3) is the only effect authority, and effects reach
  the client only as event envelopes. The client's host pump feeds
  raw bytes to `write_vt` without the scanned-actions path used by
  in-process tabs; otherwise clipboard/bell would fire twice.
  OSC 52 *read* is default-deny until HS-3 decides the remote
  clipboard story (stated now so the deferral can't drift to
  allow).
- Debug wire tracing (`RUST_LOG=roost_ipc=debug`) must redact
  effect-envelope payloads — clipboard contents are secrets and
  must not land in logs.

The full authority split, per kind of state:

| Kind | Examples | Behavior |
|---|---|---|
| Persistent server state | title, cwd, agent axes/notification state, project/tab layout | Server holds it; client hydrates on connect (lists + events) — changes that happened while detached are *state to hydrate*, not missed edge events |
| Per-attach hydrated state | screen, modes, cursor, kitty-keyboard stack | The snapshot payload |
| Edge-only client effects | bell, OSC 52 clipboard write, desktop banner, pointer shape | Event envelope to the leased client; dropped (logged) when none — pointer shape is an existing engine `OscAction` and follows this row |
- OSC color/theme queries are answered from the server VT's palette.
  The palette is seeded with defaults at spawn and re-seeded from
  the attached client's theme on connect (replacing the in-process
  UI-theme seed); an app that set its own colors keeps them.

## 7. Headless size and environment

- Detach does not resize; PTYs keep the last attached size (no
  SIGWINCH into TUI agents). Tabs created while detached use a
  configured default (120×40). **Shipped in HS-1a** — the 120×40
  default is live for tabs a session creates (restored or freshly
  opened via `roostctl`); the "configured" half (a config key to
  change it) is still deferred, D7's default is hardcoded today.
- PTY env is today's (`TERM=xterm-256color` forced, `COLORTERM`,
  `ROOST_TAB_ID`), with `ROOST_SOCKET` = the session socket — Claude
  hooks and `roostctl` inside session tabs work unmodified.
- Resize authority: last-writer-wins from the connected client
  (takeover means exactly one).

## 8. Lifecycle, locks, security

**Implemented by HS-1a** — start/fork/setsid, the flock + stale-socket
+ `(dev, ino)` + directory-hygiene posture, the readiness pipe,
same-UID peer check, and stop's order/probe/never-self-exits/
stopping-client poll semantics below all shipped as written. The one
deviation is the client-notification step of Stop, noted inline.

- **Start:** `roost-session start` (or spawn-if-missing from the
  client): single fork + `setsid`, parent exits; launch cwd passed
  as a consumed-once env hint; stdio → null; logs to the session
  profile's log dir via the existing appender pattern. The fork
  happens **before any tokio runtime thread exists** (fork in a
  threaded process is the classic trap; the engine bootstrap makes
  it tempting to stand the runtime up first). The spawning client
  connect-retries with a timeout after daemonizing — a losing
  racer's flock failure just means the winner is serving. State and
  log files under the session profile carry the same 0600 posture
  as the socket.
- **Login-session caveat:** a setsid daemon spawned from a GUI app
  stays in the user's systemd scope; on `KillUserProcesses=yes`
  distros logout kills it, silently defeating the feature. Plain
  daemon first, but user docs must say logout ≠ detach on such
  systems and point at `loginctl enable-linger` /
  `systemd-run --user --scope`; a proper `systemd --user` unit is
  HS-2/3 hardening.
- **Locks & sockets:** reuse `single_instance.rs` flocks on the
  session profile. Stale socket: connect-probe before unlink; record
  `(dev, ino)` at bind and unlink only an owned socket at shutdown.
  Mode `0600` **plus** same-UID peer check on accept — mode bits are
  insufficient once the socket is SSH-forwarded — **plus directory
  hygiene** (the discovery note had this and it must not get lost):
  the runtime dir is created-or-validated user-owned `0700`,
  symlinked or world-writable parents rejected, the bound inode
  verified. Never TCP. Debug builds use a distinct profile dir so a
  dev session can't collide with the real one.
- **Readiness:** the daemon signals its parent through a readiness
  pipe only after lock + directory validation + bind succeed, so
  spawn-if-missing reports real success/failure instead of racing;
  a concurrent spawner that loses the flock just reconnects to the
  winner.
- **Stop:** `session.stop` → refuse new tabs/attaches → notify
  clients (events envelope; data conns close with a labeled ERROR —
  **shipped in HS-1b**, closing HS-1a's plain-close deviation: events
  connections get the terminal `session.stopping` envelope with
  `reason: "stop"`, data connections an `ERROR shutting-down`, both
  best-effort under a 2 s deadline with EOF as the accepted fallback
  when the peer has stopped reading; the labeling happens *before* the
  relays are aborted, or the peer would get a bare EOF it could not
  tell from a crash)
  → flush `state.json` (existing `Workspace::flush`) → **awaited**
  `shutdown_all(deadline)` reap of every child (SIGHUP, waitpid
  loop, SIGKILL escalation — the current `terminate_child` is a
  fire-and-forget background watchdog and is not sufficient here:
  process exit must not outrun reaping) → close listeners → unlink
  owned socket. The stopping client polls
  until the socket is unreachable **or answers with a different
  `session_id`** — reachability alone would misread a racing
  spawn-if-missing as "stop failed". SIGTERM/SIGINT take the same
  path. The server never self-exits.
- **Crash:** PTY children die with the server (they are its
  children; no daemon-of-daemons). `state.json` write-through means
  layout survives; scrollback does not (disk history is explicitly
  out of scope — secrets). Client sees connection loss → host shown
  disconnected → reconnect spawns a fresh session with the same
  layout, fresh shells — exactly today's relaunch semantics, but
  scoped to the host.

## 9. Client architecture (iced)

> **Shipped in HS-2 (plan 037), with a recorded deviation**: the seam
> did not become an app-scoped `TabBackend::Host` variant — the real
> cut turned out to be `TabHandleKind::Host` plus `HostConnSet`; see
> the roadmap's HS-2 section and
> [`docs/development/host-sessions.md`](../docs/development/host-sessions.md)
> for what actually landed. The policy list below (reply handling,
> OSC-scan mode, theme reseed, test hooks) held as specified.

- `TabBackend` (HS-0): the enum/trait behind which `TerminalTab`
  does `attach / send_input / send_resize / has / foreground_cwd`
  (14 call sites) — **plus the backend-dependent policies that
  differ between modes and must be part of the seam from day one**:
  reply-buffer handling (in-process: drain client-VT replies to the
  PTY; host: discard — §6), OSC-scan mode (in-process: scanned
  attach; host: unscanned, effects via events), OSC color reseed on
  theme change (in-process: reseed the drain; host: send the theme
  to the server), and the test-mode input-capture hooks. Cutting
  the seam at five methods and rediscovering these in HS-2 would
  mean re-cutting a landed abstraction. `InProcess` wraps today's
  `TabSession`; `Host` wraps a handle from `HostConn`.
- **Host-qualified identity (HS-0):** today's `EngineFeed`, tab
  map, and events identify tabs by bare `i64` — which collides the
  moment a second id-space exists (local + a host, two hosts, or a
  restarted server reusing ids). HS-0 introduces
  `TabKey { host, tab_id }` / `WorkspaceKey { host, project_id }`
  (local = a distinguished host value) plus the server
  generation/session id, and moves the tab map, active selection,
  feed variants, navigation, and stale-event rejection onto those
  keys. Deferring this to HS-2 would force a second redesign of
  every event and tab API.
- `HostConn` (HS-2): owns the control/events/data connections for
  one host, runs on the existing tokio runtime, and feeds the
  existing `EngineFeed` channel via one new pump: host events →
  workspace-event variants the drain already understands, keyed by
  `TabKey`; PTY frames → the same `TabOutput::Bytes` path
  (`servicing.rs` doesn't care where bytes came from);
  connection-state changes → a new feed variant for the Hosts UI.
- Sidebar: Hosts section hidden until the first saved host exists;
  selecting a host swaps the project/tab views to that host's
  workspace (state cached client-side from events + list ops, the
  server authoritative). Local stays untouched above it.
- Verbs: Connect (palette), Disconnect (close connections; also
  what quitting does), Stop session (explicit, confirmed). Localhost
  auto-reconnect on launch once opted in.
- Platform gating: until a Mac `roost-session` ships, the Mac build
  hides the localhost connect entry (cfg-gated) rather than
  offering a spawn that cannot work — HS-2 must not ship a visible
  dead end on one of its two client platforms. SSH targets appear
  on both platforms at HS-3.
- Reconnect/backoff: on connection loss, host marked disconnected
  with a retry affordance; automatic retry only for localhost.

## 10. SSH transport (HS-3)

**Shipped in full: HS-3 slice 1, transport (plan 038); HS-3 slice 2,
bootstrap (plan 039).** The transport described below landed as
designed, with one detail sharper than sketched here: the scratch
directory holding the generated `ssh_config`, the `ControlMaster`
control socket, and the local bridge socket is claimed per connect
**attempt** (`roost-ssh-<host_id>-<pid>-<seq>`), not per saved host —
a fresh attempt sweeps its host's older directories first (reclaiming
this process's own outright, probing another process's `bridge.sock`
before touching it, fail-safe on anything live or unclassifiable)
rather than one directory being reused or fought over across
overlapping lifecycle paths. See
[`docs/development/host-sessions.md` → Transport: SSH hosts](../docs/development/host-sessions.md#transport-ssh-hosts)
for the shipped shape and
[`docs/reference/paths.md`](../docs/reference/paths.md#ssh-scratch-directories)
for the exact paths. The **bootstrap** half sketched below (detect/
install/verify a remote `roost-session`) also shipped, considerably
more elaborate than the sketch — a whole candidate ladder rather than
two rungs, a staged verify-before-commit install with incumbent
backup/restore, and an install→stop→await-gone→start upgrade order
the sketch's "install, then re-verify" glossed over; see
[`docs/development/host-sessions.md` → Bootstrap: install/upgrade over SSH](../docs/development/host-sessions.md#bootstrap-installupgrade-over-ssh)
for what actually landed. OSC 52 clipboard policy over SSH is settled
(writes forward to the attached client's own machine under its
`clipboard-write` setting; reads stay dropped everywhere, SSH included
— [`config.md`](../docs/reference/config.md#clipboard-write)); remote
image paste stays deferred (roadmap open questions).

Stdio-mux, Herdr's shape — no remote socket forwarding to manage:

```text
 client ── UDS ── local bridge ── ssh -T host 'sh -c "…exec roost-session client-bridge"'
                                        │ (the shipped remote command tries an
                                        │  eight-rung candidate ladder — see
                                        │  the bootstrap section linked above)
                                        │ (per accepted connection,
                                        │  shared ControlMaster)
                              far side: connect to local session UDS,
                                        pump stdio ↔ socket
```

- One `ssh -T` exec per accepted connection (control ×2, events,
  each tab) with `-o RequestTTY=no` pinned explicitly, multiplexed
  over a private per-host `ControlMaster` with a short
  `ControlPersist` window; disconnect tears the master down with an
  explicit `ssh -O exit`. Generated ssh config includes the user's
  config **first** (their settings win), then fallback keepalives.
  The bridge specs half-close: remote EOF on stdout closes the
  local socket write side and vice versa, so clean shutdowns aren't
  read as errors.
- The stdio bridge is CI-able without sshd: run the far-side bridge
  subcommand over local pipes and drive the same protocol — a
  cheap lane that keeps the transport tested outside the shed.
- Bootstrap on first connect, as shipped: `uname` platform detect →
  an eight-rung candidate ladder (one definition generating both the
  probe's discovery script and the transport's exec chain) → an
  **install** rule requiring `app_version`, `session_protocol` and
  `libghostty_build` to match exactly — deliberately stricter than the
  unchanged runtime attach gate (protocol + payload kind + build, no
  `app_version`) — → staged install to `dest.tmp.$$` via `tee`,
  verify-before-commit, incumbent backed up and restored on a
  post-commit failure → warn (don't edit dotfiles) if not on PATH.
  Non-interactive runs (`roostctl`, IPC-originated connects) never
  prompt and never mutate. Cross-platform (Mac client → Linux host)
  installs from a checksum-verified release asset, on stable tags
  only — a prerelease client never auto-downloads.

## 11. Failure modes

| Failure | Behavior |
|---|---|
| Tee forwarder overflow (slow client/link) | Server closes that data conn with ERROR; client resumes from seq via the replay ring (no re-snapshot) or falls back to a full re-attach with backoff. Server VT unaffected (fed synchronously). |
| Client resize mid-attach | Geometry rides `tab.attach`, so the common case has no mid-attach resize. Residual server-side resize (takeover race) costs remaining history pages — re-attach recovers. |
| Snapshot decode error / truncated or corrupt SNAP | ERROR path → re-attach. Decoder poisoning is per-attach, not per-tab. |
| Version / payload-kind mismatch | Clean `session.identify` error naming both versions; nothing binary ever sent. Localhost after a package upgrade is the *common* instance — HS-2's "Restart session" flow (§4.4). |
| Tab process exits mid-attach | Server completes the SNAP stream, flushes pending PTY frames, then sends EXIT as the final frame — EXIT is always last on a data conn. (The engine's existing deadline path can emit Exit before trailing bytes internally; the data plane reorders so EXIT is terminal.) |
| Takeover mid-attach | Prior client's data conn closes mid-SNAP (ordinary close/ERROR path); its control/events conns close with a `taken-over` envelope. |
| `session.stop` during attach | Clients get a labeled shutdown envelope on events; data conns close with ERROR(shutting-down), not bare EOF. (**Shipped in HS-1b**; EOF stays the fallback for a peer that has stopped reading.) |
| Tab task panic | Tab marked dead, EXIT-equivalent to attached clients, rest of session unaffected; crash-report path per existing engine crash handling. |
| Session crash | Children die; layout persisted; client shows host disconnected; reconnect = fresh shells, same layout. |
| Client crash / quit | Session unaffected (this is the feature). |
| Client disconnect between handshake and READY | Server aborts the forwarder and discards the attach token; no partial state. |
| SSH drop | Host disconnected; explicit reconnect (auto for SSH considered in HS-4). |
| Stale socket after crash | Connect-probe → unlink → bind. `(dev,ino)` guard prevents cross-deletion. |
| Second client connects | Takeover: **all** prior-client connections closed (control included) with a labeled envelope/ERROR; stale ops get `taken-over`. |

## 12. Testing architecture

HS-1a shipped the control-plane subset of this section: the Rust
contract tests in `crates/roost-session/tests/`
(`session_lifecycle_test.rs` for start/stop/reap ordering,
`session_events_test.rs` for subscribe over the real serve path,
`socket_guard_test.rs` for the `(dev, ino)` stale-socket guard,
`session_hydration_test.rs` and `session_osc_drain_test.rs` for
headless hydration + drain, `session_fast_exit_test.rs` for
pre-attach exits still closing their rows), the batch/fence/stop
contract tests in `crates/roost-engine/tests/`
(`workspace_batch_test.rs`, `events_push_test.rs`,
`session_ops_test.rs`, `pty_shutdown_test.rs`) plus the
`session_daemon`-marked pytest module
(`tools/roosttest/test_session.py`) and its required `session-e2e` CI
job.

**HS-1b shipped the rest of this section**: the attach/data-plane/perf
items called out below (fence on the tab task, resume-from-seq,
snapshot byte fidelity, exactly-once replies, the
input-latency-under-`tab.dump` budget) are covered by
`crates/roost-engine/tests/tab_task_test.rs` +
`attach_forwarder_test.rs`, the Rust integration client
`crates/roost-session/tests/attach_stream_test.rs` (the only place
GHOSTSNP is semantically decoded — Python never reimplements it), and
the new `tools/roosttest/test_session_attach.py` contract lane
(+ `dataplane.py`, a frame reader and record-*tag* scanner only), all
on the same `session_daemon` marker and the same required `session-e2e`
job. Four items from the lists below are **deferred, not done**, with
destinations: large-paste throughput over an artificially delayed link
→ HS-3 (needs a link-shaping harness); the package-upgrade-while-
running restart flow → HS-2 (§4.4 assigns the UI flow there);
client-side gap/duplicate/generation rejection and the resize-withhold
state machine → HS-2's client (the wire rules are documented, and the
server side is proven gap-free here); colliding integer ids across
hosts → HS-2 (needs two hosts and a client).

- **Rust unit tier** (repo convention, `_test.rs` under `tests/`):
  the §5 record-boundary buffering (split headers, partial records,
  truncated CRC) — shipped as plan 034's
  `crates/roost-vt/tests/snapshot_test.rs` battery (fragmentation
  down to 1-byte feeds, corruption/truncation, caps, continuation
  round-trips); fence assignment on the tab task (including the
  HS-0→HS-1 seq relocation — a test pins fence semantics across the
  move — **shipped in HS-1b**); the resize-withhold
  state machine (**deferred to HS-2 with the client**); **exactly-once
  replies** (DA/DSR/color query answered once by the server, client
  reply buffer verifiably discarded — the two-VT design reintroduces
  the doubled-reply bug class `osc_drain_reply_test.rs` exists for, so
  it gets the same treatment — **shipped in HS-1b**, proven from both
  sides); `Error::from_result` coverage for any new codes.
- **HS-1 (no UI):** a roosttest module speaks both planes from
  Python — control ops via the existing IPC client, the data plane
  via a small binary-frame reader. **HS-1a shipped the control-plane
  half** as `tools/roosttest/test_session.py` (the `session_daemon`
  marker, required `session-e2e` CI job): start/stop/status,
  identify, the events atomic-batch fence and resync, stale-socket
  recovery, hydration, and OSC drain are covered there and in the
  Rust tests listed above. Everything below that names `seq`, attach,
  takeover, or the data connection **shipped in HS-1b** as
  `tools/roosttest/test_session_attach.py`, except the four deferrals
  named at the top of this section. Contract tests: fence
  correctness (no lost/duplicated bytes around `seq`), READY-before-history
  ordering, PTY-frame priority, resume-from-seq (ring hit and ring
  miss), a fast-producer/slow-consumer soak proving no attach
  thrash, takeover (including stale-control-op rejection),
  stop-vs-disconnect, stale-socket recovery, **handshake rejection
  tests** (wrong protocol_version, unknown kind, mismatched
  libghostty_build — lie about each and assert the clean error),
  disconnect between handshake and READY, EXIT during attach,
  kill -9 mid-attach, events resync after a forced gap, input
  latency flat while a large `tab.dump` runs on the control
  connection. Byte fidelity: attach, replay frames into a fresh
  client-side Terminal via test-mode ops
  (`tab.feed_pty_bytes`/`tab.dump_resolved` pattern), compare to
  the server's `tab.dump` — and a snapshot-decode fidelity case
  (dump of the decoded client Terminal equals the server's dump at
  the fence). Note `tab.dump`/`tab.dump_resolved` are served today
  where the VT lives (the UI); on the session they run against the
  **server** Terminal — HS-1 work — and the snapshot-decode
  fidelity case goes through a small Rust integration client (the
  same `roost-vt` wrapper the UI will use) rather than
  reimplementing GHOSTSNP in Python. Additional contract cases from
  review: arbitrary fragmentation at transport and record
  boundaries; fence landing mid-UTF-8/CSI/OSC/DCS (continuation
  round-trip); seq gap/duplicate/generation-mismatch rejection;
  bounded server memory under a non-reading client; colliding
  integer ids across local + two hosts; concurrent daemon starts;
  package-upgrade-while-running (the §4.4 restart flow);
  stale-lease input rejected immediately after takeover. Perf
  budget in HS-1 acceptance: localhost attach of a 2000-line tab
  reaches READY < 500 ms; N concurrent attaches bounded (exact N in
  the plan); input latency and large-paste throughput measured over
  an artificially delayed link. **Shipped in HS-1b** as median-of-3
  under 500 ms × `ROOST_TEST_TIMEOUT_SCALE`, N = 8 distinct tabs
  within twice that, and an input-latency case measured during a large
  `tab.dump`; the delayed-link large-paste measurement is the HS-3
  deferral above. The session lane got **its own job** (`session-e2e`,
  marker-based, no enumerated CI lists to update) with its own path
  filters — decided in HS-1a and unchanged by HS-1b, which only added
  a module to `SESSION_E2E_TESTS` and a ghostty build step now that
  `roost-session` links `roost-vt/ffi`.
- **HS-2:** the same lane, now driving the real iced client against
  a session (the harness already launches either target); screenshot
  smoke for the Hosts chrome.
- **HS-3:** shed VM (`linux-test` skill; sshd on port 2222 exists) —
  full bootstrap + attach from a Mac client into the VM; CI keeps
  the localhost lanes, shed covers SSH manually until a CI story
  exists.

## 13. Open questions (carried, not blocking)

- ~~Exact event-envelope revision/resync wire shape~~ **Resolved,
  shipped in HS-1a**: atomic `{revision, events: [...]}` batches, one
  per commit including empty ones — see
  [ipc.md's `events.subscribe`](../docs/reference/ipc.md#eventssubscribe).
- ~~Whether `session.connect` carries the client theme seed or a
  separate op does (§6)~~ **Resolved, shipped in HS-2**: a separate
  `session.set_theme` op — `session.connect` still takes `{takeover}`
  only, and the client sends its theme after connect (and on theme
  commit).
- ~~Data-plane priority implementation detail (two queues vs one with
  priority) and the exact forwarder/ring byte budgets~~ **Resolved,
  shipped in HS-1b**: one pump, no second queue — the snapshot and the
  tee are weighed against each other per frame (PTY leads; SNAP is
  guaranteed a turn after 256 KiB of PTY payload or 50 ms). Budgets:
  2 MiB replay ring, 8 MiB forwarder queue, 512 MiB / 60 s per attach.
  See [ipc.md's Data plane](../docs/reference/ipc.md#data-plane).
- Remote image paste policy over SSH (HS-3 plan; OSC 52 read is
  already pinned default-deny).
- Mac `roost-session` packaging (post-HS-3; code is portable
  already).
