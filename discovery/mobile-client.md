# Roost on mobile: a Flutter client for host sessions

**Status: discovery, 2026-10-27. Direction decided on 2026-10-28.**
The MVP will be built on a feature branch *after* the protocol-7
release ([protocol-7-release.md](protocol-7-release.md)). That release
lands the prerequisites on `main`, each of which is also a desktop
improvement:
- finding the session (G1);
- tab size ownership (G2), which **supersedes the Fit/Mirror design in
  [Sizing](#sizing-the-one-real-protocol-question)**;
- tab environment by opener;
- OSC 7501 program status;
- Ghostty main with Zig 0.17;
- the first `roost-client` extraction (B1).

[Decisions from the 2026-10-27/28 review](#decisions-from-the-2026-1027-28-review)
lists what changed. A mockup canvas is at
https://claude.ai/artifact/BncmJWCp6miDoBij1gHaPS (private). Pursuing
this would
amend roadmap decision
[D1](host-sessions-roadmap.md#d1--client-and-products), which puts any
web or mobile client out of scope. That amendment, and any
`docs/development/vision.md` decision-log entry, belong in the PR that
builds it, not here.

Claims are checked against the code at `fd6ccd41` or against the
Android spike run on 2026-10-27 ([What the spike proved](#what-the-spike-proved)).
Anything inferred and not run is marked **(unverified)**.

## The goal

A standalone Roost app for Android, with iOS left open. You add hosts
the way Roost-Iced does, then see and drive each host's projects and
tabs, with a real terminal, from a phone, a tablet or a foldable. The
app is optimized for large screens first and collapses down to phones.
Where the desktop relies on hotkeys, the app offers touch-native
controls instead. It is a client of `roost-session`, just as Roost-Iced
is when it shows a host, so it adds no new op set. It reuses the
control-plane session ops and the data-plane attach protocol that
`roost-ipc` already defines.

**Decisions taken in the planning conversation:**

| Question | Decision |
|---|---|
| UI toolkit | Flutter. Rust for everything below the pixels, reused from this workspace. |
| Product | A standalone app in this repo. shed-mobile may adopt parts later; the shed duplication isn't a concern now. |
| Version skew | For the MVP, an exact `session_protocol` match is enough, and the app is built from the same commit as the `roost-session` it talks to. [Not a one-way door](#version-coupling). |
| Platforms | Android first. Keep iOS possible: nothing Android-only in the shared layers. |
| Form factors | Tablets, foldables and phones are all in scope. Lay out for the large screen first. |
| SSH transport | In-process SSH in Rust (russh) behind a `Dialer` seam, so the transport can change later ([why](#ssh-transport)). |

### Decisions from the 2026-10-27/28 review

These amend the table above and the sections below.

| Question | Decision |
|---|---|
| Build approach | The real MVP order, not a throwaway proof of concept. The prerequisites land on `main` first as the protocol-7 release. Then the MVP is built on a feature branch. |
| Size mode | **No mode switch.** Protocol-7 ownership applies: a phone that switches to a tab, types, taps, scrolls or copies takes the size. When it goes to the background it sends `release`, and the size goes back to the desk. Switching to a tab on the phone resizes it immediately, even just to read; Charlie chose that on 2026-10-28. This supersedes the Fit/Mirror modes and the `tab.write`-for-everything workaround in [Sizing](#sizing-the-one-real-protocol-question). The compose box and quick replies still use `tab.write`, which never takes ownership. Typing on the soft keyboard goes over the data plane and does. |
| Background notifications | The MVP is **foreground only**, but build the seams: a process-scoped `RoostApp` and runtime that a later foreground service can keep alive, and a `Notifier` trait whose MVP implementation is in-app only. |
| Test devices | Samsung Galaxy S25 (compact) and Galaxy Tab S6 Lite (about 800 dp in portrait, about 1333 dp in landscape; roughly medium and expanded). Develop on the Flutter Linux desktop target resized across the breakpoints, plus emulators with matching custom hardware profiles. The foldable tabletop layout drops to nice-to-have. |
| Soft keyboard | **Samsung Keyboard** is the default on both devices, so the keyboard spike tests it before Gboard. The Tab S6 Lite is the low-end floor for painter performance. |
| Quick replies | Keys, not words: 1/2/3, Enter, Esc, arrows. A "Yes" chip would type the word into a numbered menu. |
| Inbox text after a reconnect | OSC 7501 `blocked` records carry `kind` and `msg`, and protocol 7 exposes them on the agent record. That gives a durable "why" after a reconnect, which the live-only `notification.fired` can't. |
| Toolchain | Ghostty main needs Zig 0.17. The spike built Android with 0.16. M1 of the protocol-7 release re-checks the `aarch64-linux-android` build with 0.17. |
| Prior art | Superlogical's Rex ships a native iOS app with iPad and Duo layouts. Its mobile client originally relied on client-local reflow ("Authoritative server has no idea"). Rex later adopted the owner model this release adapts. See `../superlogical/research/notes/2026-10-27-rex-update.md`. |

**Out of scope for the MVP:**

- Installing or upgrading `roost-session` on a host (bootstrap).
  Compatibility windows.
- Tabs that run on the device. A phone has no PTY to offer, and the app
  only shows hosts.
- Push notifications through a relay server.
- Syncing the host list with a desktop's `state.json`.
- Embedding the app's terminal in shed-mobile. That may come later, and
  the [crate layout](#crates) keeps it possible.

The project-wide non-goals still hold: no split panes, and no browser
host.

## What the spike proved

All of it ran on this Pop!_OS box against the Android emulator. The
device was the `Pixel_9_Pro_Fold` AVD: x86_64, Android 17 (API 37),
16 KB pages. The build used NDK 28.2.13676358 and Zig 0.16.0, at the
Ghostty pin `f2d5758f…` from `third_party/ghostty/build.sh:22`. Every
artifact was built under `/tmp/craze`; the repo was not modified.

| # | Step | Result |
|---|---|---|
| 1 | `zig build -Demit-lib-vt=true -Doptimize=ReleaseFast -Dcpu=baseline -Dtarget={aarch64,x86_64}-linux-android` in the pinned Ghostty checkout, with `ANDROID_NDK_HOME` set | Both built in about 34 s each, with no source changes. Output was `libghostty-vt.a` plus `.so`. |
| 2 | An unmodified copy of `crates/roost-vt` built with `--features ffi` for both Android triples. bindgen ran against the NDK sysroot via `BINDGEN_EXTRA_CLANG_ARGS_<triple>`. | It compiled and linked every test binary for both triples. |
| 3 | Pushed `roost-vt`'s 16 test binaries to the emulator with `adb` and ran them | **222 passed, 0 failed, 1 ignored.** The ignored test is the `#[ignore]` perf capture in `crates/roost-vt/tests/snapshot_test.rs:1587`. Snapshot encode/decode, key and mouse encoders, render state, selection and `vt_dump` all pass on Android. |
| 4 | `cargo check -p roost-ui-model -p roost-ipc --target x86_64-linux-android` in the real workspace | It passes. There are two unused-import warnings in `crates/roost-ipc/src/peer.rs:17,19`, because `target_os = "android"` takes the fallback branch. Under `-D warnings` these would fail. |
| 5 | russh 0.65 with the `ring` backend and with the default `aws-lc-rs` backend, built for both Android triples with the NDK clang as `CC_<triple>` | Both built. The spike binaries were 4.0 MB (ring) and 4.6 MB (aws-lc), arm64 release, unstripped. |
| 6 | From the emulator, russh connected to stock OpenSSH in a throwaway Docker container (Ubuntu 24.04). It authenticated with an ed25519 key it had generated itself, and opened two concurrent exec channels on one connection. One channel was used as a duplex `AsyncRead + AsyncWrite` stream (`Channel::into_stream`). | Both backends passed: key exchange, public-key auth, the duplex echo, and exit status. The SHA-256 host-key fingerprint is available in `check_server_key`. |
| 7 | **Full stack, from the emulator.** russh connected to OpenSSH, and an exec ran `roost-session client-bridge` against a real `roost-session` (a dev build at `6d9c12d`). On the control plane it called `session.identify`, `tab.list` and `tab.write`. On a second exec it called `roost-ipc`'s `DataConnection::handshake`, attaching with `focus: false` and `kinds: [ghostty-snapshot]`. libghostty's `SnapshotDecoder` decoded the snapshot, and `RenderState` walked the viewport. Then it sent a data-plane `INPUT` frame. | Everything worked. The session and the client both reported `ghostty-f2d5758f6305867d+snapshot.v1`. The attach was accepted as `ghostty-snapshot` at 120×40. The decoded viewport showed the shell output, and the `INPUT` round-tripped through the shell. |
| 8 | The sizing facts behind [Sizing](#sizing-the-one-real-protocol-question) | `tab.write` and the `focus: false` attach left the tab at 120×40, which the attach reply showed after the `tab.write`. After one `INPUT` frame, `tab.dump` reported 60×20, the geometry the client had declared. |

**Not covered by the spike:**

- A physical arm64 device. Only the arm64 link was proven.
- Flutter and flutter_rust_bridge themselves.
- iOS.
- Performance. The emulator's ~0.7–1 s SSH handshake is its slirp
  network, not a device measurement.
- The `remote_command()` binary-search ladder over russh. The spike
  exec'd an absolute path instead.
- Tailscale on Android.

## Architecture

```text
┌──────────────────────────────── Flutter (Dart) ────────────────────────────────┐
│ layout by window size class · terminal painter · soft/hardware keyboard ·      │
│ extra-keys bar · compose box · selection handles · notifications · settings    │
└──────────────────▲──────────────────────────────────────┬──────────────────────┘
       view models, frames (bytes)                commands, input
                   │      flutter_rust_bridge (opaque handles, StreamSink)
┌──────────────────┴──────────────────────────────────────▼──────────────────────┐
│ roost-mobile  (new, cdylib)  FRB surface; owns each attached tab's            │
│               roost_vt::Terminal + RenderState on one owner thread;           │
│               encodes dirty rows; maps platform key events → KeyEncoder       │
├────────────────────────────────────────────────────────────────────────────────┤
│ roost-client  (new, lib)  host registry · connection state + compatibility    │
│               gate · mirror · reconnect ladder · op queue · attach/hydration  │
│               ─ all behind  trait Dialer                                       │
├──────────────┬────────────────┬──────────────────────┬────────────────────────┤
│ roost-ipc    │ roost-vt (ffi) │ roost-ui-model       │ Dialer impls:           │
│ wire types,  │ libghostty-vt  │ palette, keybind,    │  UnixDialer (desktop dev│
│ IpcClient,   │                │ rollup, theme,       │   against a local       │
│ DataConnection│               │ notification_inbox… │   session)              │
│              │                │                      │  SshDialer (russh)      │
└──────────────┴────────────────┴──────────────────────┴────────────────────────┘
```

The shape follows the project's north star. The Rust core owns all
state and logic. Flutter renders the core's events and sends commands
back. It is never a second source of truth.

### Crates

**`roost-ipc` (existing): small changes.**

- `IpcClient` and `EventStream` are hard-wired to
  `tokio::net::UnixStream` (`crates/roost-ipc/src/client.rs:57-78,
  258-277`). Make them generic over the stream, or have them store boxed
  `AsyncRead`/`AsyncWrite` halves behind an `over_split(r, w)`
  constructor. `DataConnection::handshake` is already generic
  (`crates/roost-ipc/src/client.rs:466-516`), and the spike drove it over a russh channel
  unchanged.
- Silence or fix the Android `cfg` in `peer.rs`. Android supports
  `SO_PEERCRED`, so including it in the Linux branch is probably right
  **(unverified)**.
- The crate is `publish = false` (`Cargo.toml:60`), but shed pins it by
  git rev, so an API change ripples into shed and shed-mobile on their
  next bump.

**`roost-client` (new): extracted from `roost-iced`.**

The iced host client lives inside the binary crate, under
`crates/roost-iced/src/`. `host_conn.rs` and `host_conn/*` hold about
7.0k production lines, and `app/host_tab.rs` about 1.46k. There are
about 245 inline tests. The extraction map uses paths relative to
`crates/roost-iced/src/`:

| Module | Coupling to iced | Class |
|---|---|---|
| `host_conn/state.rs` (connection state machine, compatibility gate, `Backoff`) | None. It says "Pure: no sockets, no clock". | Moves as is |
| `host_conn/mirror.rs` (workspace projection) | None | Moves as is |
| `host_conn/reconnect.rs` (ssh ladder policy) | None | Moves as is |
| `host_conn/queue.rs` (`HostOps`, intents, attach permits) | None | Moves as is |
| `host_conn/upload.rs` | Dials a path | Moves behind `Dialer` |
| `host_conn/task.rs` (prologue, connect loop, event pump) | `crate::engine_feed` (`crates/roost-iced/src/host_conn/task.rs:53`), `session_actions::ActionEvent` (`:314-319`), path dials (`:826, :1327, :1386`), local daemon spawn | Moves after the feed becomes a `HostSink` trait, the paths become a `Dialer`, and spawn becomes optional |
| `host_conn.rs` (`HostConnSet`: incarnations, tunnels, outage scheduler) | `EngineFeedSender` and a runtime handle (`:583-584`), `Arc<SshTunnel>` (`:891-895`) | Same seams, plus `Transport::establish() -> Dialer` in place of `SshTunnel` |
| `app/host_tab.rs` (attach and hydration: `Hydrator`, deferred PTY queue, seq fence, `ResumePoint`) | Five `TerminalTab` calls (`grid`, `hydration_terminal`, `write_vt`, `swap_terminal`, `resize_for_host`), feed self-wakes, ambient `tokio::spawn` | Moves behind an `AttachTarget` trait that iced's `TerminalTab` implements |

The proposed event API keeps today's shape: `HostEvent` mirrors the
`EngineFeed` host variants (`State`, `Workspace`, `ConnectFacts`,
`Emptiness`, `Tab(TabKey, HostTabFrame)`, plus the self-wakes).
`HostConnSet`'s methods are already the command API.

**Three invariants must survive the move.**

1. All of a host's events go through a single FIFO, and the client is
   re-entered on one owner thread. Incarnation attribution and the
   "decoder is dropped on the main thread" rule depend on both.
2. **Drop order.** `HostConnSet` and the host attaches must drop before
   the runtime they run on, because their `Drop` signals runtime tasks
   to wind down. iced gets this from field order
   (`crates/roost-iced/src/app.rs:3413-3423`). Whatever owns them in
   `roost-mobile` (`RoostApp` below) needs the same field order, or an
   explicit `Drop`.
3. Under `local-backend = session`, which a fresh Roost-Iced install
   uses, every desktop user's local tabs run over this same
   `HostConnSet`.

The extraction is therefore a desktop-wide regression surface, not a
mobile-only change.

**Options for when to extract:**

| | Approach | For | Against |
|---|---|---|---|
| A | Extract everything first: one mechanical commit, iced moved onto it, gated by `e2e-iced` | One implementation from day one | The largest, riskiest step comes before any mobile value. The test fixtures have to come off `engine_feed` first. |
| B | Move the four pure modules now (state, mirror, reconnect, queue), with iced importing them from `roost-client`. Write a mobile-shaped task and attach in `roost-client` against `Dialer`. Converge iced's `task.rs`/`host_tab.rs` onto it after the MVP. | The tricky pure logic (compatibility gate, mirror folding, retry policy) has one copy immediately. The first move is low risk. | For a while, two connection tasks live in the repo. |
| C | Write the mobile client inside `roost-mobile` | Fastest start | It becomes a third implementation, counting shed-core's. This is the drift the crate exists to prevent. |

**Leaning: B.** Its convergence step should be an explicit milestone
with an exit criterion, not a hope.

**`roost-mobile` (new): the FRB crate.**

This is the only crate that knows about Flutter. It should probably be
excluded from the workspace's `default-members`, so that `make check`
doesn't pay for flutter_rust_bridge codegen and cdylib builds
**(unverified)**. A sketch of its surface:

```rust
// opaque, Send handles; every async entry point runs on one persistent runtime
pub struct RoostApp;                       // owns roost-client's HostConnSet + key store access
impl RoostApp {
    pub fn hosts(&self, sink: StreamSink<HostsView>);          // registry + state + rollups
    pub fn add_host(&self, label: String, target: SshTarget) -> Result<HostId>;
    pub fn connect(&self, host: HostId);  pub fn disconnect(&self, host: HostId);
    pub fn workspace(&self, host: HostId, sink: StreamSink<WorkspaceView>); // projects/tabs/agent state
    pub fn op(&self, host: HostId, op: WorkspaceOp) -> Result<()>;         // project.*, tab.open/close/...
    pub fn attach(&self, tab: TabKey, mode: SizeMode) -> TerminalHandle;
    pub fn trust_host_key(&self, host: HostId, fingerprint: String);
}
pub struct TerminalHandle;
impl TerminalHandle {
    pub fn frames(&self, sink: StreamSink<FrameReady>);   // "dirty" wake-ups, coalesced
    pub fn take_frame(&self) -> Vec<u8>;                  // binary dirty-row payload, pulled per vsync
    pub fn key(&self, ev: KeyInput);  pub fn text(&self, s: String);  pub fn paste(&self, s: String);
    pub fn scroll(&self, rows: i32);  pub fn mouse(&self, ev: PointerInput);
    pub fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16);
    pub fn send_line(&self, s: String);                   // compose box → tab.write (never resizes)
    pub fn selection(&self, ...);  pub fn copy(&self) -> String;
}
```

This reuses shed-mobile's proven patterns:

- One persistent tokio runtime. FRB's per-call executor would drop a
  long-lived pump (`../shed-mobile/rust/src/api/bridge_rt.rs`).
- Opaque handles with an idempotent `teardown()` and a `Drop` backstop.
- Leak counters that tests assert on.
- Committed generated code with a CI drift check.

**The Flutter app: `mobile/` at the repo root**, a sibling of `mac/`.
It uses Riverpod and a vendored cargokit, as shed-mobile does. It needs
Linux and macOS desktop targets for the dev loop, plus Android. iOS can
be scaffolded but isn't built.

### Build plumbing

The spike needed these changes, done by hand outside the repo:

1. `third_party/ghostty/build.sh` needs an optional target. It should
   pass `-Dtarget=<triple>` and write to a per-target prefix
   (`out/<triple>/`), with the host build staying at `out/` so nothing
   that exists today moves. An Android target needs `ANDROID_NDK_HOME`.
   Ghostty finds the sysroot itself and links its `.so` with 16 KB max
   page size (`src/build/GhosttyLibVt.zig:271-276`). Ghostty's NDK host
   table only knows `linux-x86_64`, `darwin-x86_64` and
   `windows-x86_64`. That's fine on this Linux box, and should be fine
   on Apple Silicon too, because the NDK ships its macOS binaries
   under the `darwin-x86_64` directory name **(unverified on a Mac)**.
2. `crates/roost-vt/build.rs` hard-codes `third_party/ghostty/out`
   (`crates/roost-vt/build.rs:40`). It should pick `out/<TARGET>/` when cargo's
   `TARGET` differs from `HOST`. bindgen needs the NDK sysroot when
   cross-compiling, which the spike supplied through
   `BINDGEN_EXTRA_CLANG_ARGS_<triple>`. Whether cargokit sets that is
   **(unverified)**; if it doesn't, build.rs can derive it from
   `ANDROID_NDK_HOME`.
3. Cargokit runs cargo once per ABI from Gradle, so the Zig build has
   to have happened first. A `make mobile-deps` that runs `build.sh` for
   each Android triple is simpler than teaching Gradle about Zig.
4. Our cdylib statically links `libghostty-vt.a`. It needs a 16 KB
   page-alignment check, as shed-mobile does in
   `../shed-mobile/scripts/check-16kb-alignment.sh`. NDK r28+ aligns to 16 KB by
   default **(unverified for our link)**.

## SSH transport

**Recommendation: russh in Rust behind a `Dialer` trait.** The reasons:

- **No listening socket.** shed-mobile's dartssh2 design binds a
  loopback TCP port and execs `client-bridge` for each accepted
  connection (`../shed-mobile/lib/ssh/roost_tunnel.dart:103`). Android
  apps share one loopback interface, so any app on the device with
  network permission can connect to that port **(platform behavior,
  not tested here)**. Behind it is a `roost-session` control socket,
  and a session authenticates nothing beyond the peer UID
  (`crates/roost-session/src/serve.rs:369-375`). Through `client-bridge`
  that check always passes, so `tab.open` with any argv is reachable.
  russh channels are in-process streams, so there's no port to reach.
  shed-mobile has the same exposure today, and that's worth fixing
  there too.
- **The hot path stays native.** Terminal output goes from the network
  to Rust to libghostty to frames. With dartssh2, decryption would run
  in pure Dart on the UI isolate before crossing into Rust.
- **One connection, many channels.** Control, events, one channel per
  attached tab, and uploads are all exec channels on one SSH
  connection. The spike ran two at once.
- **Testable with cargo.** The spike's Docker OpenSSH container, or the
  real-sshd harness in `tools/session/live/`, can exercise the Android
  code path's logic on the host.

**The seam:**

```rust
#[async_trait] // or a boxed-future fn; shape to taste
pub trait Dialer: Send + Sync {
    async fn dial(&self) -> io::Result<Box<dyn Duplex>>; // Duplex = AsyncRead + AsyncWrite + Send + Unpin
}
```

- `UnixDialer(PathBuf)` connects to a local socket. This is the desktop
  dev loop against a local `roost-session`, with no SSH.
- `SshDialer` holds a russh handle and opens a channel that execs
  `roost_ipc::ssh::remote_command()`, the same binary-search ladder the
  desktop uses (`crates/roost-ipc/src/ssh.rs:383`,
  `crates/roost-ipc/src/bootstrap.rs:290-351`).
- If shed-mobile ever embeds this, a third implementation could take
  bytes from Dart. It should use a Unix socket in the app-private
  directory, not loopback TCP.

**Crypto backend:** `ring`. Both backends built and ran on Android in
the spike. ring produced the smaller binary and has no CMake or NASM
path to go wrong on other hosts.

**Keys and trust:**

- Generate an ed25519 key in the app; russh did this in the spike. Keep
  the private key in `flutter_secure_storage`, which is Keystore-backed,
  and hand it to Rust at connect time. Importing an existing key is
  optional.
- Add Host shows the public key with Copy and Share, for pasting into
  `authorized_keys`.
- Host keys use trust-on-first-use with the SHA-256 fingerprint shown,
  pinned per `host:port`. A changed key fails hard and never retries,
  matching the desktop ladder (`crates/roost-iced/src/host_conn/reconnect.rs:130-154`).
- Map russh errors onto the existing `SshFailure` classes
  (`crates/roost-ipc/src/ssh.rs:671`), so the shared reconnect policy
  keeps its evidence. That is the main cost of leaving the `ssh`
  binary, whose stderr the desktop classifies today.
- There is no `~/.ssh/config` and no agent. Tailscale MagicDNS names
  should work as plain TCP once the Tailscale app's VPN is up
  **(unverified)**.

**No session on the host.** `client-bridge` writes
`client-bridge: no session is listening at <path>…` to stderr and exits
1 (`crates/roost-session/src/bridge.rs:42-52`). The app can classify
that, as the desktop does (`crates/roost-ipc/src/ssh.rs:681`), and offer **Start session**.
That is one exec of `roost-session start`, which prints `ready pid=N`
or `already-running` (`crates/roost-session/src/main.rs:28-31`). It
needs a resolved path, because there's no ladder for `start`, so a
small ladder variant is required. This is not the bootstrap flow, which
stays out of scope.

## Sizing: the one real protocol question

> **Superseded on 2026-10-28.** Protocol 7's size-ownership rules
> ([protocol-7-release.md](protocol-7-release.md) M3) answer this
> section. User-initiated input takes ownership, foreign resizes are
> announced with `tab.resized`, and the owner leaving hands the size
> back. The facts below describe protocol 6 and are kept as the
> reasoning that led there.

Facts, from the code and confirmed by spike step 8:

| Action | Resizes the PTY? |
|---|---|
| Attach with `focus: true` | Yes, to the client's grid, before the snapshot is encoded (`crates/roost-engine/src/attach.rs:205-215`) |
| Attach with `focus: false` | No |
| Data-plane `INPUT` | **Yes.** Every frame carries the connection's declared geometry (`crates/roost-engine/src/attach.rs:595-597, 950-962`). |
| Data-plane `RESIZE` | Yes |
| Control-plane `tab.write` | **No.** It carries no geometry (`crates/roost-engine/src/tab_task.rs:413-416`). |
| Someone else resizes after you attached | **Nobody is told** (`docs/reference/ipc.md:4036-4041`). The only signal is polling `tab.dump`'s `cols`/`rows`. |

So a phone that types into a desktop's tab shrinks it, and the desktop
draws at the wrong size until it interacts. The reverse also happens:
the desktop's background resize wave resizes tabs the phone is using
(gap G2 in [sessions-everywhere.md](sessions-everywhere.md#g2-window-size-with-several-clients)).

**Client-side modes that work on today's server:**

1. **Fit.** The app attaches with its own grid and takes the size when
   you type. It's simple, and right when the device is the only one
   looking. On an unfolded foldable or a tablet the grid is close to
   desktop width, so the fight matters less.
2. **Mirror.** The app attaches with `focus: false` at the PTY's grid
   (`snapshot_cols`/`snapshot_rows` from the attach reply) and pans and
   zooms locally. **It must not send data-plane `INPUT`.** The
   connection's declared geometry never updates when another client
   resizes the tab, and the server applies that geometry ahead of each
   `INPUT` (`crates/roost-engine/src/attach.rs:950-962`). So a
   keystroke sent after someone else's resize would put the old size
   back. Instead, Mirror sends *all* of this client's input (encoded
   keys, pastes, mouse reports) over control-plane `tab.write`, which
   carries no geometry. One control connection is served serially
   (`docs/reference/ipc.md:694-697`), so the client's own input stays
   in order. The costs:
   - Keystrokes share the host's sequential op queue with other ops,
     so latency is higher **(unmeasured)**.
   - The local view still goes stale after a foreign resize. That
     affects only what you see, not the tab. The app notices by
     polling `tab.dump` or by re-attaching until a resize event
     exists.
3. **Line input over `tab.write` in Fit mode.** The compose box and
   quick replies send through the control plane, which never resizes.
   In Fit mode, ordering between this and data-plane `INPUT` from the
   same client isn't guaranteed, since they are separate connections
   into one tab task. That's fine for whole lines, and it's wrong for
   interleaved keystrokes **(inferred)**.

**Leaning:** Fit on expanded layouts and Mirror on compact ones, each
with a one-tap toggle. The compose box and quick replies always use
`tab.write`. If `tab.write` keystroke latency turns out to be too high,
Mirror waits on the server-side resize event below. Data-plane `INPUT`
in Mirror is only safe once the client is told about foreign resizes.

**Server-side work that would make Mirror solid.** It isn't needed for
the MVP, and it overlaps G2.

- **A `tab.resized {tab_id, cols, rows}` event.** Clients ignore unknown
  event names (`crates/roost-ipc/src/client.rs:334-339`,
  `crates/roost-iced/src/host_conn/mirror.rs:111-115`), and
  `docs/reference/ipc-compatibility.md:282-285` says new events don't
  bump the protocol. A new data-plane frame type *would* need a bump,
  because unknown frames are fatal in both directions
  (`crates/roost-engine/src/attach.rs:1007-1011`, `crates/roost-ipc/src/client.rs:452-454`).
- G2 option 2, a server-side rule that a background `tab.resize` from a
  connection that isn't attached is ignored while another connection is
  attached.
- The desktop's watch-first attach (G2 option 1) is a parity change in
  iced, not something the app needs.

## Terminal rendering in Flutter

- Rust owns the `Terminal` and `RenderState` on one owner thread.
  `Terminal` is `Send` but not `Sync` (`crates/roost-vt/src/terminal.rs:297-300`).
  Dart never touches libghostty.
- **The frame protocol is pull-based.** Rust coalesces PTY writes and
  sends a "dirty" wake-up. A Flutter `Ticker` pulls one binary payload
  per vsync: the dirty rows as runs of `(col, len, fg, bg, flags,
  utf8)`, plus cursor, selection and scrollbar.
  `roost_vt::resolve::{RenderedRow, DrawCell}` already produce
  resolved, sparse rows (`crates/roost-vt/src/resolve.rs:16-46`).
  Sending a `Uint8List` instead of per-cell FRB structs keeps FFI cost
  flat.
- **The painter** is a `CustomPainter` per terminal. It draws background
  rects in runs, plus text runs from per-row cached `Paragraph`s rebuilt
  only for dirty rows. A glyph atlas (`drawRawAtlas`) is the escape
  hatch if full-screen TUI redraws are too slow **(unverified: the
  biggest performance unknown)**.
- **Fonts:** bundle JetBrains Mono from `third_party/jetbrains-mono`,
  the same default as iced, so cell metrics and glyph coverage match the
  desktop. Pinch-to-zoom changes the font size, which changes the grid,
  so it must debounce the same way window resizes do.
- Kitty graphics stay blank after attach, as on the desktop (G5).

## Input

- **Hardware keyboard** (tablet covers, Bluetooth): map Flutter
  `KeyEvent` to `roost_vt::Key` plus mods, then to the same `KeyEncoder`
  the desktop uses, synced from the terminal's modes. The desktop
  keybind table in `roost-ui-model::keybind` applies as-is, so tablets
  with keyboards get hotkeys back.
- **Soft keyboard:** a custom `TextInputClient`, with autocorrect and
  suggestions off and a sentinel-buffer trick so backspace on an empty
  field still reaches the terminal. This is what Termux-style terminals
  do. **This is the biggest UX risk and needs its own early spike.**
  Android IMEs differ: Gboard composition, voice typing, swipe.
- **Extra-keys bar** above the soft keyboard: Esc, Tab, sticky Ctrl and
  Alt, arrows, `| ~ / -`, PgUp/PgDn. Take it from shed-mobile's
  `../shed-mobile/lib/features/terminal/terminal_keys.dart`: `ExcludeFocus` so taps
  don't hide the keyboard, and sticky Ctrl that only touches a single
  ASCII letter. Hide it when a hardware keyboard is attached.
- **Compose box.** Write a whole prompt in a native field, with
  autocorrect, voice and multi-line editing, then send it as a bracketed
  paste (`roost-ui-model::bracketed_paste`) over `tab.write`. For
  driving agents from a phone this is probably the most valuable mobile
  element.
- **Touch:**
  - Tap focuses the terminal and shows the keyboard.
  - Long-press starts a selection with handles, using `roost-vt`'s
    selection and `word_selection`.
  - One-finger drag scrolls local scrollback, or sends wheel reports
    when the program enabled mouse tracking (`ScrollRoute`).
  - Two-finger drag pans in Mirror mode.
- **Share to Roost:** an Android share intent uploads with
  `session.put_file` (10 MiB per file, `docs/reference/ipc.md:3647-3743`)
  on its own channel, then pastes the returned path into the tab. That
  is exactly what iced does for a host tab
  (`crates/roost-iced/src/host_conn/upload.rs`,
  `crates/roost-iced/src/app/file_transfer.rs:835-846`).

## Mobile UX

**Layout by Material 3 window size class.** Build the expanded layout
first, then collapse it.

| Class | Width | Layout |
|---|---|---|
| Expanded | ≥ 840 dp (tablet landscape, unfolded foldable) | Desktop-like: a persistent sidebar with hosts and their projects (the host bands of `roost-ui-model::host_sidebar`), a tab strip, the terminal, and an optional notification and agent panel |
| Medium | 600–840 dp | A navigation rail for hosts and projects; the tab strip and terminal |
| Compact | < 600 dp (phones, folded) | Stacked screens: hosts, then projects, then tabs, then the terminal. Swipe between a project's tabs. |

**Elements that replace hotkeys:**

- **A palette button.** The palette is a toolkit-neutral model
  (`crates/roost-ui-model/src/palette.rs:1-8`) that builds actions from
  `KeybindAction`, so it becomes the "anything a hotkey did" entry
  point. Note that `palette.*` ops are UI-socket ops and aren't served
  by a session. The app runs its own palette from the shared model.
- **Agent quick replies.** When `agent_report.changed` or
  `has_notification` says a tab is waiting, show chips (Yes, No, Enter,
  Esc, 1/2/3) above the keyboard. They are sent with `tab.write`.
- **Notification inbox** (`roost-ui-model::notification_inbox`). Tapping
  an entry jumps to the tab. Acknowledge with `tab.clear_notification`
  (`docs/reference/ipc.md:1318-1349`). `notification.fired` and `tab.effect` are
  live-only and are left out of event replay (`docs/reference/ipc.md:3104-3126`), so
  after a reconnect the app rebuilds pending state from `tab.list`'s
  `has_notification`.
- **Tab overview with text previews** on expanded layouts. `tab.dump`
  serves a tab's text without an attach (`docs/reference/ipc.md:818-821`), so each
  host keeps one attach while the overview stays cheap. Live previews
  would need warm tabs (#566).
- **Workspace management** through the session ops `project.create`,
  `project.rename`, `project.delete`, `project.reorder`, and `tab.open`,
  `tab.close`, `tab.set_title`, `tab.reorder`. There's no op to move a
  tab between projects (`crates/roost-ipc/src/messages.rs:3932-3960`).
  New tabs open with `activate: false`, so they don't steal the
  desktop's focus.

**Foldables:**

- Fold and unfold resize everything at once. Debounce grid changes the
  way the desktop's background wave does (a 150 ms settle,
  `crates/roost-iced/src/app/background_resize.rs:35`), and decide
  Fit or Mirror per posture.
- In the half-folded tabletop posture, put the terminal on the top half
  and keys plus the compose box on the bottom. Flutter exposes the
  hinge as `MediaQuery.displayFeatures` **(unverified in this app)**.
- Handle multi-window and split-screen resizing without recreating the
  activity.
- Dev loop: resize the Linux desktop build to cross breakpoints. The
  local `Pixel_9_Pro_Fold` AVD covers posture changes; add Pixel Tablet
  and a phone AVD.

**Lifecycle.** Android drops sockets in the background. The protocol
already handles reconnects well:

- `events.subscribe {from_revision, session_id}` resumes from a
  1024-commit / 4 MiB ring (`docs/reference/ipc.md:3092-3102`).
- Data-plane resume (`server_epoch`, `tab_generation`,
  `resume_from_seq`) falls back to a fresh snapshot on a miss
  (`docs/reference/ipc.md:4163-4202`).

Reconnecting on resume is therefore cheap. Background notifications
need a foreground service (shed-mobile has one in
`../shed-mobile/lib/services/foreground_service.dart`). That is an opt-in setting, not
MVP.

## Version coupling

The MVP refuses any session whose `session_protocol` differs from its
own constant, and it is built from the same commit as the
`roost-session` it ships alongside.

- `SESSION_PROTOCOL_VERSION` has been `6` since 2026-09-15
  (`crates/roost-ipc/src/messages.rs:2436`).
- A Ghostty pin mismatch doesn't refuse the session; it falls back to
  the `vt` payload kind, at lower fidelity.
- It isn't a one-way door. The check is the client's own policy, and
  widening it later only adds code.

The cost is friction: a host upgrade and an app update have to go
together. Keep the check in one place in `roost-client` (it's already
`check_compatibility` in `crates/roost-iced/src/host_conn/state.rs:172`), and make its
message name the host's version and the app's version.

## Testing

| Layer | How |
|---|---|
| `roost-client` | The inline tests move with it, fixtures reworked onto a test `HostSink`. Integration tests run against a real `roost-session` over `UnixDialer`, and the same tests run over `SshDialer` against the Docker OpenSSH container the spike used or `tools/session/live/`. |
| `roost-vt` on Android | Spike step 3 as a CI job: build the test binaries for `x86_64-linux-android` and run them on an emulator. Cheap insurance across Ghostty pin bumps. |
| `roost-mobile` | Rust tests for the frame encoder and key mapping. Leak counters for handles and pumps. |
| Flutter | Widget tests against a fake core (provider overrides). Golden tests for the terminal painter. Integration tests on the Linux desktop target under Xvfb against a local `roost-session` (shed-mobile's `make test-integration-linux` pattern). Marionette driving for agents (`stridelabs_drive`). |
| Devices | Emulator profiles for tablet, foldable and phone. One physical arm64 device before any release. |

`tools/roosttest` drives UIs over their IPC socket. The app has no
socket, so parity tests would need a different hook. That isn't needed
for the MVP.

## A possible MVP order

*Amended on 2026-10-28.* The protocol-7 release runs first, on `main`.
It covers step 2's first part (B1, its M5), the server-side work in
step 7 (the `tab.resized` event, now with ownership), and the G1 fix
from the open questions. So the feature branch starts at step 1's
mobile-only plumbing (build targets, `peer.rs`, the stream-generic
`IpcClient`, the Android CI job), then continues with the rest of step
2, the `Dialer` and the minimal connection task. "Fit and Mirror
modes" in step 4 no longer exist.

1. **Plumbing.**
   - Target-aware `build.sh` and `build.rs`.
   - Fix the `peer.rs` Android warnings.
   - Make `IpcClient` and `EventStream` stream-generic.
   - Add a CI job that builds `roost-vt` for Android, and optionally
     runs its tests on an emulator.
2. **`roost-client`, step B1.**
   - Move state, mirror, reconnect and queue, with iced importing them;
     `e2e-iced` stays green.
   - Add `Dialer` and `UnixDialer`.
   - Write a minimal connection task (prologue, mirror, reconnect) and
     an attach/hydration path behind an `AttachTarget`. `roost-mobile`
     owns each attached tab's `roost_vt::Terminal`.
   - Tests run against a local session.
3. **The Flutter shell on the Linux desktop, against a local session.**
   - FRB scaffolding with cargokit.
   - The expanded layout: sidebar, tab strip, terminal.
   - Watch-only attach and the painter.
4. **Input.**
   - Hardware keys through `KeyEncoder`.
   - A soft-keyboard spike, then the real implementation.
   - The extra-keys bar, compose box, selection and copy, scrolling.
   - Fit and Mirror modes.
5. **SSH and devices.**
   - `SshDialer` with russh/ring, a key store, trust-on-first-use, Add
     Host, and Start session.
   - Android builds for tablet, foldable and phone.
   - The compact and medium layouts.
6. **Mobile niceties.** Notification inbox, quick replies, Share to
   Roost, tab previews.
7. **After the MVP.** Converge iced onto `roost-client` (step B2). Add
   the server-side `tab.resized` event and the G2 background-wave rule.
   Consider iOS.

**MVP exit criterion.** From a tablet and from a phone on the same
tailnet, you can:

- add a host by name and target;
- trust its host key;
- see its projects and tabs with live agent state;
- attach to a tab and type into it, through hardware keys, the soft
  keyboard and the compose box;
- answer an agent prompt with a quick reply;
- survive backgrounding and a network change without losing the tab.

And the desktop's open copy of that tab hasn't been resized unless the
user chose Fit.

## Risks

| Risk | Why it matters | Mitigation |
|---|---|---|
| Soft-keyboard input in Flutter | Android IME behavior varies, and a terminal is not a text field | Spike it first in milestone 4. Fall back to a Kotlin `InputConnection` through a platform view if Flutter's text input can't be tamed. |
| Painter performance on full-screen TUI redraws | Agents redraw a lot | Pull-per-vsync with dirty rows. Glyph-atlas fallback. Measure early on a real device. |
| Extraction regresses desktop users | `HostConnSet` carries local tabs under `local-backend = session` | Option B's small first step. A mechanical move with no logic changes. `e2e-iced` gating. |
| Owner-thread invariants broken by an FFI caller | Dart can call from anywhere | `roost-mobile` serializes every call onto one owner task per host. Dart never holds a `Terminal`. |
| Protocol lockstep friction | Every protocol change means the app and every host move together | Acceptable for the MVP; revisit a compatibility window once the app is in daily use |
| russh failure evidence is thinner than ssh stderr | The reconnect policy classifies failures | An explicit error→`SshFailure` mapping with tests |

## Reuse from shed-mobile

| What | Where |
|---|---|
| FRB runtime, opaque-handle and leak-counter patterns | `../shed-mobile/rust/src/api/bridge_rt.rs` |
| Cargokit (vendored, with Gradle 9 and toolchain-pin patches) | `../shed-mobile/rust_builder/` |
| Extra-keys toolbar, sticky Ctrl, focus reports | `../shed-mobile/lib/features/terminal/terminal_keys.dart` |
| Secret storage abstraction (secure storage on mobile, 0600 file on desktop, memory in tests) | `../shed-mobile/lib/storage/secret_store.dart` |
| In-app key generation, identity store | `../shed-mobile/lib/keys/` (ported to Rust if keys move there) |
| Host-key pinning UX | `../shed-mobile/lib/ssh/host_key_store.dart` |
| Foreground service | `../shed-mobile/lib/services/foreground_service.dart` |
| 16 KB alignment check, lockfile rev guard, CI layout, Play release flow | `../shed-mobile/scripts/`, `.github/workflows/` |

## Open questions

- **Naming and identity.** What app id should it use (the Mac bundle is
  `ai.stridelabs.Roost`), and which Play listing?
- **Host list sync.** Should the app import hosts from a desktop's
  `state.json` (`HostSnapshot`, `crates/roost-engine/src/persistence.rs:175-187`),
  or stay independent?
- **Default size mode per form factor.** *Resolved:* there are no modes;
  the protocol-7 ownership rules apply. Rex's local-only reflow on the
  phone is a possible later experiment.
- **G1 exposure.** *Resolved:* protocol-7 M2 moves the Linux session
  socket under HOME. If a host runs Tailscale SSH and its session was
  started from the desktop, the phone's `client-bridge` looks for the
  session in the wrong place (sessions-everywhere G1). Fix it in the
  session (option A or D) before the app ships, or document it?
- **Where keys live.** *Leaning (2026-10-27):* generate keys in Rust
  (russh, as the spike did), so SSH stays in one language. The
  alternative was Dart (shed-mobile's pinenacl path), which reuses
  existing code.
- **iOS.** Background networking is far stricter there. Is "reconnect on
  foreground" enough for iOS v1? Ghostty already builds an iOS
  xcframework (`src/build/GhosttyLibVt.zig:148-215`).
- **D1.** If the MVP happens, the roadmap's D1 and the vision's
  non-goals need amending in the same PR.

## Evidence (2026-10-27)

- **Toolchain:** NDK `28.2.13676358` at `~/Android/Sdk/ndk/`, Zig
  `0.16.0`, Rust `1.97.1` with `aarch64-linux-android` and
  `x86_64-linux-android` targets. The AVD was `Pixel_9_Pro_Fold`, with
  system image `android-37.1/google_apis_playstore_ps16k/x86_64`;
  `getconf PAGE_SIZE` returned 16384.
- **libghostty-vt:** the two `zig build … -Dtarget=<triple>` runs wrote
  `/tmp/craze/gvt-{aarch64,x86_64}-android/lib/libghostty-vt.a`
  (18.6 MB and 18.2 MB).
- **roost-vt on the emulator:** 16 binaries; 222 passed, 0 failed, 1
  ignored (the perf capture).
- **Full stack from the emulator:** `session.identify` returned
  `session_protocol 6` and
  `libghostty_build "ghostty-f2d5758f6305867d+snapshot.v1"`. The attach
  reply was `AttachAccepted { kind: "ghostty-snapshot", mode: Snapshot,
  snapshot_cols: 120, snapshot_rows: 40, … }`. The snapshot decoded to
  1889 bytes and its viewport showed the `tab.write` output. After
  `INPUT`, the viewport showed `typed-via-INPUT-frame`, and a later
  `tab.dump` reported `"cols":60,"rows":20`.
- **russh:** the Docker OpenSSH container's host key was shown as
  `ssh-ed25519 SHA256:…`. Auth returned `success=true` with the
  generated key. Two channels ran concurrently, with exit status 0.
