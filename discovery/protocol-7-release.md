# The protocol-7 release: sessions everywhere, Ghostty main, program status

**Status: decided, 2026-10-28; panel-reviewed the same day (Codex
astra, GLM 5.3, CodeRabbit). Not started.**

This is the source of truth for the next release. Each milestone below
runs as its own plan (`/flows:gauntlet`). Every milestone plan cites
this file and does not re-decide anything in it. If a plan must depart
from a rule here, it says so and updates this file in the same PR.

**Inputs:**
- [sessions-everywhere.md](sessions-everywhere.md): gaps G1 and G2,
  plus G9, added by this release.
- [mobile-client.md](mobile-client.md): the mobile client, which builds
  on this release.
- Superlogical's Rex, as prior art: the research notes in the sibling
  checkout `../superlogical/research/notes/2026-10-27-rex-update.md`,
  which is not part of this repo. Everything this plan uses from them
  is restated here.

## Why

Roost's host sessions let several machines attach to one
`roost-session`. A real setup showed where the design stops short:
- The Linux desktop can't find a session started over Tailscale SSH,
  because the socket path depends on `XDG_RUNTIME_DIR`.
- Two screens on one tab fight over its size. The last one to
  interact wins, and the other draws garbage.
- Every tab inherits the environment of whichever SSH connection
  started the daemon.

Meanwhile three things have moved:
- The agents Roost tracks now report their own status (OSC 7501).
- Ghostty main is 921 commits past Roost's pin.
- The planned mobile client needs all of the above first.

This release fixes those on `main`, under one protocol bump.

## What a user gets

1. **One session per machine (per user and build), reachable from
   anywhere.** Start work over SSH from the laptop, sit at that
   machine's own screen (the "HDMI desk"), and see the same tabs. On
   Linux, a session started from the laptop is no longer invisible to
   the desktop, and the desktop no longer reports "couldn't start".
   (G1)
2. **Several screens on one tab, without size fights.**
   - A screen that is only showing a tab never resizes it.
   - Doing something deliberate at a screen takes the size: switching
     to the tab, typing, clicking, copying, scrolling, or resizing the
     focused window.
   - The other screen shows the tab at that size, cleanly, with a
     "sized by X" marker and a Take size action.
   - When the screen that set the size leaves, the size passes to a
     screen that is still attached. (G2)
3. **Tabs behave correctly for where they were opened.**
   - A tab opened at the desk can launch GUI apps and use the desk's
     ssh-agent.
   - A tab opened from the laptop knows it is remote, and can't reach
     the desk's display.
   - Neither inherits the environment of whatever started the daemon.
     (G9)
4. **Agent status reported by the agent itself (OSC 7501).** Claude
   Code and Codex report working, blocked (with the reason) and done
   directly. The sidebar, the inbox and remote clients get a "why"
   that survives a reconnect, and interrupts show up immediately.
5. **Per-tab memory,** in the op set and in `roostctl`.
6. **Ghostty main** (Zig 0.17). This also lays the groundwork for
   Kitty graphics (#645), scrollback search (#167) and history
   compression (#366).

## Release rules

### Protocol

**The session protocol goes 6 → 7 exactly once per release.** This
uses the documented "unreleased generation may be amended in place"
exception (`docs/reference/ipc-compatibility.md`, Version bumps;
precedent: protocol 6, 2026-09-15). In practice:

- **M1's bump commit moves every mirror together.** That means:
  - the Rust constant and its doc;
  - the Swift test constant and its `session.identify` vector-name
    construction;
  - `tools/roosttest/dataplane.py`;
  - the iced host-connection check;
  - the value quoted in `ipc.md`;
  - the new `session.identify.response.v7.json` vector.

  Tests look up the current generation's vector by name, so this
  can't be deferred.
- **Every later milestone re-cuts, in its own PR, any vector whose
  shape it changes.** Intermediate commits stay green.
- **At release:** a corpus re-check, plus one CHANGELOG bullet
  ("Session protocol 6 → 7") that describes the resulting contract.
- **A downstream consumer** (shed pins `roost-ipc` by git rev) does not
  re-pin between M1 and the release cut.
- **Dev sessions** built from different amendments of 7 are
  incompatible. After pulling a milestone, restart the debug-namespace
  daemon (`roost-session-dev`).

### The frozen Swift app

**Swift is feature-frozen; compatibility maintenance is allowed.**

- **What Swift is:** `Roost.app` serves only the UI socket. It is not
  a session client, so the session bump doesn't touch its runtime.
- **What must still pass:** `swift build`, `swift test` and
  `make e2e-mac`, all against the new pin.
- **Mechanical updates are expected:** the session constants and
  vector names that Swift's tests pin, and the agent fixtures (see M1).
- **Wire additions must not break Swift.** Swift rejects unknown
  params and top-level keys:
  - `mac/Sources/Roost/IPCHandlerImpl.swift:1247-1268`
  - `mac/Sources/Roost/IPCMessages.swift:221-229`

  So any new request field is sent only to targets that accept it
  (session targets), and is `skip_serializing_if`-absent otherwise.
  The precedent is v0.0.20's `tab.open` unknown-field retry.

### Merging and gates

**Each milestone is one or more PRs into `main`.** Each is green on
`ci-success` and merged by hand. The release is cut only after all five
milestones have landed.

**Gates are derived from CI, not from this document.** Each milestone
runs the `make` targets of every CI job that its changed paths trigger
(`.github/workflows/ci.yml` path filters). The usual set:

- **On the Pop!_OS dev box** (Linux): the component targets. Bare
  `make check` also runs Swift, which that box can't. The targets:
  - `make test-rust`
  - `make check-iced`
  - `make test-harness`
  - `make e2e-iced`
  - `make e2e-session`
  - each host lane, one at a time, never concurrently
- **On the mac-mini over SSH** (see the `mac-test` skill):
  - `cargo test` for the touched crates. macOS-only failures exist,
    such as socket paths overrunning the Unix length limit.
  - `swift build`, `swift test` and `make e2e-mac`, whenever
    `third_party/ghostty/**` or the IPC contract changes.
  - `make e2e-iced`.

### The live session

**Don't break the live session.** The dev box's own `roost-session`
hosts the agents doing this work.
- Never stop or restart it.
- Remove `ROOST_SOCKET`, `ROOST_TAB_ID` and `ROOST_AGENT_HOOK` from the
  environment of gates and lanes. Otherwise test runs talk to the live
  session.
- Debug builds use the separate `roost-session-dev` namespace. Use
  that, or a private override, for hand checks.

## Milestones

| # | Milestone | Protocol | Lands after |
|---|---|---|---|
| M1 | Ghostty main + Zig 0.17, OSC 7501, per-tab memory | 6 → **7** | — |
| M2 | G1: the Linux session socket under the home directory | none | M1 (so the old-path daemon in `e2e-old-session` is a different generation) |
| M3 | G2: tab size ownership, client identity, the bridge preamble | amends 7 | M1 |
| M4 | G9: tab environment by opener | amends 7 | M3 (needs client identity and the bridge preamble) |
| M5 | B1: host-client modules into `roost-client` | none | M3 (edits `mirror.rs`, `host_tab.rs`) |

**Release:** after M1–M5 have all landed, plus a `release.yml` dry run
with Zig 0.17. Its `vendored-ghostty-*` caches also start cold, and
`ci-success` never runs that workflow. Then the mobile client gets its
own feature branch.

---

### M1. Ghostty main, Zig 0.17, program status, per-tab memory

**Status: merged: #647, #648.**

Two PRs, in order. Landing the pin bump on its own keeps any churn in
`swift-mac` or the byte-exact VT tests separate from feature changes.

#### M1a. The pin bump (its own PR)

**Move the pin** from `f2d5758f` to Ghostty main `6ba14415`, the commit
the 2026-10-27 diff audit reviewed. A newer tip is allowed only if it
passes the same audit: no change to any C API that roost-vt or Swift
uses, and the snapshot envelope still v1.

**Files to update:**
- `third_party/ghostty/build.sh`: `GHOSTTY_SHA`, its comment, and the
  Zig version guard (`^0\.16` → `^0\.17`);
- `.mise.toml`: zig `0.17.0`;
- every doc that says 0.16:
  - `README.md:77`
  - `docs/getting-started/installation.md:125`
  - `docs/development/setup.md:14`
  - `CLAUDE.md` (Build: "zig `0.16.x`").

**CI:** it installs Zig through `jdx/mise-action`, so no workflow edits
are needed. The first run starts from cold `vendored-ghostty-*` caches.

**Fallback, only if Zig 0.17 blocks:** pin `b58ef1d67`, the last
commit that builds with 0.16. It has OSC 7501 rev 0.4, but not the
malformed-id fix `8c425a7bd` or the snapshot decoder's unknown-record
skip `781c58e96`.

**Expected breakage:**
- `libghostty_build_matches_current_pin` (`crates/roost-vt/src/lib.rs`).
- Byte-exact `vt_dump`, selection and key/mouse encoder tests.
- `vt_dump`'s padding and background refill, against the new
  formatter. The formatter now:
  - keeps trailing blank rows;
  - restores erased backgrounds;
  - emits OSC 8 hyperlinks;
  - handles origin mode and pending wraps.
- `vt_dump`'s module doc, which says hyperlinks aren't carried.
- Comments that cite the old pin's contract, in
  `crates/roost-engine/src/osc.rs`, `crates/roost-osc/src/lib.rs` and
  `crates/roost-vt/tests/write_pty_test.rs`.

**`vt` fallback:** any pin change changes `libghostty_build`, so a
client and a session on different builds fall back to the `vt`
payload. Protocol 7 forces restarts at release anyway.

**Probe silence:** with the new pin and no program-status callback
installed, a terminal must not answer `OSC 7501 ; ?`. This keeps
`Roost.app` silent, so Claude Code there falls back to hooks.

**Mobile check (non-blocking):** build `libghostty-vt` for
`aarch64-linux-android` with Zig 0.17, as `mobile-client.md`'s spike
did with 0.16, and record the result. A failure doesn't block M1; it
gets an issue.

#### M1b. Program status and per-tab memory (bumps the protocol to 7)

**Evidence (2026-10-28).** Pseudo-terminal captures of the installed
agents, and source reads of the others, summarised here.

| Agent | 7501 | Probes first | Seen |
|---|---|---|---|
| Claude Code ≥ 2.1.302 | yes | yes; silent without a reply | `idle`; `working`, whose msg is an activity summary; `blocked` with kind=permission, whose msg is the command awaiting approval; `done`; three `clear`s on exit. No child records seen in the 2026-10-28 captures (*amended 2026-10-28:* this row said "up to 32 background-task child records (inferred)"; a background task produced none) |
| Codex ≥ 2026-10-09 (#52725) | yes | no | `idle`; `working`; `blocked` with kind=permission and no msg; `done`. `clear` on exit **and on Ctrl-Z**. An idle→working→idle flash in the first ~0.7 s |
| Grok CLI ≥ 1.0.70 | yes (`app=grok-build`) | yes | `blocked` with kind=permission at startup in an untrusted folder; `working` with an activity msg; `done` with a summary msg; `clear` on exit. *Amended 2026-10-28:* the first draft had no Grok row; the captures show it speaks 7501 |
| opencode 2.x, crush ≥ 0.98, pi ≥ 1.1 | in source | mixed | not captured; installed versions predate support |
| gx, cursor-agent | no | — | gx sends OSC 9;4 only |

Esc mid-turn produces `idle` within ~0.1 s for both Claude and Codex.
Hooks have no interrupt event (#622).

The full decoded record is `discovery/agent-program-status.md`, and
`tools/agent-status/` re-runs it.

**Blocking first step:**
1. Update every installed agent, and `git pull` their sources under
   `~/projects/thirdparty/` (cleared by Charlie, 2026-10-28).
2. Re-run the captures for every agent with Roost hooks
   (`crates/roost-agent/src/{claude,codex,cursor,grok,opencode}.rs`),
   plus crush and pi. Keep the capture harness in-tree, under
   `tools/`, so it can be re-run.
3. Commit the decoded results as `discovery/agent-program-status.md`.
   Leave out prompt and model text.

The rules below hold unless a capture contradicts them. If one does,
amend this file.

##### The rules

**1. The authoritative terminal answers, per backend.**
- **Session tabs:** the tab-task terminal in
  `crates/roost-engine/src/tab_task.rs`. Only `roost-session` enables
  it (`tab_task.rs:29-34`).
- **In-process iced tabs:** iced's own `TerminalTab`, which already
  drains query replies to the PTY (`ReplyPolicy::DrainToPty`,
  `crates/roost-iced/src/app/tab_backend.rs:217-221`).
- **Host-tab mirrors:** install nothing. Their replies are discarded
  (`tab_backend.rs:200-205`).

On the authoritative terminal, install libghostty's
`GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS` callback. With it set,
libghostty answers the probe. Don't also parse 7501 in `roost-osc`.

Both backends must reach parity on 7501 and on the memory query.

**2. A separate layer, composed in roost-engine.**
- Program-status records are a fourth per-tab axis, held in
  roost-engine. They are applied the way `apply_shell_mark` is, not
  through `tab.agent_report`. That request is `deny_unknown_fields`
  (`agent.rs:133`), and the frozen Swift server serves it.
- The pure functions in `crates/roost-ipc/src/agent.rs` (`effective`,
  `effective_lifecycle`, `rank`, `apply_report`, `is_nested_claim`)
  **keep their behaviour for hook-only input**. Swift mirrors them,
  and the shared `tests/agent-state-fixtures/` (which Swift's
  `AgentStateFixtureTests` load) stay untouched.
- Program-status fixtures live inline, or in a directory Swift doesn't
  load.
- The composition sits above the pure functions. The hook-derived
  lifecycle, and with it `is_nested_claim`, is computed exactly as
  today. Records override only the final effective result.

*Amendment, 2026-10-28 (plan 083, #650).* Plan 082's D4.3 kept the hook
layer reading OSC 133 through `roost-osc` while the records read
libghostty's ordered stream. That is reversed: on a terminal with a
status sink (every session tab, every in-process iced tab) the hook
layer takes its marks from the same stream, advanced in byte order
inside the one `apply_program_status` call, and `roost-osc`'s 133 maps
to no action. The two parsers disagreed on `133;P;k=i`, `133;N`,
`133;I`, `133;A;k=s` and malformed bodies, so a banner could depend on
the read split. With one reading, a raw OSC 9 / 777 libghostty places is
judged where it falls against both layers, through 082's pairing of
roost-osc's notifications with libghostty's places. That pairing needs
the two parsers to start and end every OSC at the same bytes, so
roost-osc now follows libghostty's parser through escapes, CSIs, DCSs
and SOS/PM/APC strings, 8-bit controls included: an OSC starts where
libghostty starts one, a BEL or an `ESC` ends it whatever follows the
`ESC` (it no longer waits for an ST's `\`, which a read can split off),
and CAN or SUB cancel it. A differential test holds the two to it. A
form only one parser accepts is still judged at its chunk's end. `agent.rs`
and the shared fixtures are unchanged; the forms that count on sink
terminals are now libghostty's (`docs/guides/agents.md`, "Which marks
count"), including a known limit: a mark over libghostty's 2048-byte OSC
buffer is dropped. Host mirrors take the result from the wire; the
frozen Swift app keeps its own scanner and first-letter rule.

**3. When a record is authoritative.** Program status decides a tab's
effective state while either holds:
- **a live record exists:** `working` or `blocked`, on the root or on
  any child;
- **a root `idle`, `done` or `error` record exists, and no prompt mark
  has arrived since it was set.**

*Amendment, 2026-10-28.* A prompt mark here (and in rule 6 and the
acceptance tests) is OSC 133 **A, B or D**, not A alone. Roost's own
bash and zsh integration emits only C and D, never A, so with A alone a
crashed agent's `working` record would never drop for those users and
would stay authoritative. A, B and D is the set the hook failsafe
already uses (`agent.rs:339`).

While authoritative:
- **The state:** the highest-ranked live record decides, using
  `agent.rs`'s rank order. With no live record, the root's state
  decides.
- **The reason:** `kind`, `msg`, `app` and `title` come from that same
  winning record.
- **Idle is not running:** a root `idle` maps to TabState `None`, with
  **no fallback to the shell axis**. Otherwise an interrupted agent
  (foreground process, `Inactive`) would show as Running
  (`agent.rs:269-288`).

After a prompt mark, `done` and `error` stay *displayed* as
Finished/Failed, as the spec allows, but hooks regain authority.
*Amendment, 2026-10-28 (Charlie's decision):* "displayed" now has an
end. The next OSC 133 `C` after a prompt mark retires a kept root
`done`/`error`. Without it a killed agent's `error` would show
NeedsInput forever, and would come back after every later hook-only
agent. Starting another command is the signal that the user has seen
it. (Had this been declined, the alternative was retiring it when the
tab becomes watched, focus plus active.) So a
stale or forged record can't mute a later hook-driven agent. Any
program can print a 7501 sequence. That's the same trust level as OSC
0/2 titles and OSC 9 today, and it's accepted.

**4. Mapping.**

| 7501 | AgentLifecycle | TabState |
|---|---|---|
| working | Working | Running |
| blocked | Waiting | NeedsInput |
| done | Finished | Idle |
| error | Failed | NeedsInput |
| idle (root) | Inactive (owner kept) | None |
| clear | record removed; hooks, then shell, decide again | — |

**5. Hooks while a record is authoritative.**
- **Hooks keep identity:** ownership, session id and metadata.
- **Hook lifecycle changes are dropped.** Their `detail`/`metadata`
  merge exactly as the current `lifecycle_if` veto already does.
- **A `Claim` from a source that doesn't map to the record's `app` is
  dropped.** This is what fixes #624's detached-helper claim. A
  same-app claim updates identity only.
- **The M1 plan defines the source→app table** from the captures, for
  example `claude` ↔ `claude-code`.
- **Each drop uses a new `DropReason` variant.** It's a closed enum on
  the wire in `Tab.dropped_reports` (`agent.rs:378-388`), so the new
  variants ride protocol 7. Check Swift's decoder first.

**6. Records and lifetimes.**
- **What drops records:**
  - a prompt mark (OSC 133 A, B or D; see the amendment under rule 3)
    drops `working` and `blocked` records;
  - so does the PTY child's exit;
  - `clear` removes the addressed record.
- **No process-group monitor in M1.** Roost has none today: #519 is
  open, the only foreground-group code reads a leader's cwd on demand
  (`pty.rs:2158-2168`), and the existing failsafe is OSC 133 A/B/D
  (`agent.rs:339`). M1 doesn't build one.
- **`clear` never releases a hook owner.** Codex sends `clear` on
  Ctrl-Z. Ownership changes only through today's hook and shell paths.
- **The record cap is 256 per tab.** Past it, new child records are
  refused; the root is always accepted. A refusal is logged.

**7. Banners and attention.**
- **What raises a banner:** a change of the tab's *effective* state
  into NeedsInput (blocked or error) or into Finished (done). This is
  the same set that raises one today.
- **Content:** title from the record's `title`, else its agent label.
  Body from `msg`.
- **Dedupe:** a hook and a 7501 report for the same moment raise once,
  because only an effective-state change raises. Existing focus-based
  suppression (`crates/roost-engine/src/workspace.rs`, the watched-tab
  check) still applies.
- **Debounce:** Codex's startup flash is debounced for banners and
  attention only. The effective state and interrupts are never
  delayed.

**8. On the wire.**
- The winning record's state, `kind`, `msg`, `app` and `title` appear
  as additive fields on the agent record that `agent_report.changed`
  and `tab.list` already carry. That makes the "why" survive
  reconnects. *Amended 2026-10-28:* the first draft also named
  `tab.dump`, but it carries no agent record (`TabDumpResult`,
  `crates/roost-ipc/src/messages.rs:478`); the reason rides `Tab` and
  `agent_report.changed`.
- Children are counted, not listed, in M1. Showing them in the Agents
  panel is a follow-up issue.
- No consent step: answering the probe writes nothing into agent
  configs, which is what plan 064's consent protects. Probe answers
  turn on reports even in tabs without hook consent, and that is
  intended.

**Still open, for the M1 plan to settle with tests:**
- Claude's binary can emit OSC 133 A/C/D around turns, behind a gate
  that didn't fire in the capture. If it fires inside Roost, rule 6
  would drop records mid-turn. Capture it, and if it fires, scope the
  drop to prompts from the shell.

*Closed 2026-10-28 (Charlie's decision).* The marks fire, but only in
Screen Reader Mode (`screenReader && !fullscreen && !decstbm && terminal
!= WezTerm`). Nothing in the bytes or in libghostty's event tells them
from a shell's: the semantic-prompt struct has no command id or redraw
flag yet. So the drop is not scoped. Claude's marks are treated as
prompt marks, and the resulting sequences are pinned with tests, with
and without hooks. At turn start, `working, A, working` ends Working for
any read split, and a lone `working, A` falls back to hooks and the
shell until Claude's next report. At turn end, `C, D` ends `done`'s
authority, so it becomes a kept record and shows Finished in both
orders. The real fix is a command id upstream; it is noted, not filed.

##### Per-tab memory

Expose libghostty's `GHOSTTY_TERMINAL_DATA_MEMORY_USAGE` through the op
set, on the UI socket and the session socket, and in `roostctl`.
- The value reported is the **authoritative terminal's**: the tab-task
  terminal for session tabs, the in-process terminal otherwise.
- iced may also report its local mirror's memory, as a separately
  named field.
- The op's shape is the M1 plan's call. This is #366's "measure first"
  step.

##### Acceptance

**CI and pin:**
- All CI jobs are green on the new pin, on Linux and macOS, including
  `swift-mac` and `e2e-mac`.
- `release.yml` dry-runs with Zig 0.17.
- `Roost.app` stays silent to `OSC 7501 ; ?`.

**End-to-end tests:**
- **Session lanes:** an OSC 7501 test drives bytes through
  `tab.feed_pty_bytes` (`ROOST_TEST_MODE=1`). It asserts:
  - the probe reply, via `tab.capture_pty_input`;
  - the agent record and TabState for each state;
  - the winning reason when a child record outranks the root;
  - the reason surviving a reconnect.
- **The same test for an in-process tab,** in `ICED_E2E_TESTS`.

**Precedence unit tests:**
- hook and record arriving in both orders;
- `idle` under a foreground process giving `None`;
- a different-app claim dropped, a same-app claim kept;
- banner dedupe;
- debounce affecting banners only;
- the record cap;
- `done` surviving a prompt mark (A, B or D) while authority returns to hooks;
- `clear` on Ctrl-Z keeping the owner.

**Hand check:** on the dev box's debug namespace, a real Claude Code
turn shows blocked with its message, then done.

**Memory:** the memory query answers for both kinds of tab.

**Issues:**
- Close only what regression tests prove. Expected: the interrupt half
  of #622 and the state half of #624.
- Update #621, #623 and #519 with what changed.
- #624's fix covers 7501-capable agents only. gx and cursor-agent
  don't change.

---

### M2. G1: the Linux session socket under the home directory

**Status: merged: #653** (plan 083; #651, the one-OSC-133-parser fix
for #650, shipped beside it).

*How it shipped (plan 083 corrections to this section):*
- **No `roostctl session restart` exists.** The restart flow is the iced
  UI's Restart from its `needs-restart` prompt (reason "old socket
  path"), plus a new explicit `roostctl session stop --old-path
  <socket>`. Nothing stops an old-path session on its own.
- **The old-path probe fails closed.** It never runs under
  `ROOST_SESSION_SOCKET_DIR` or `ROOST_TEST_MODE`, or on a debug build
  using the release names. The exception is the single scratch
  candidate a debug build names with `ROOST_TEST_OLD_SESSION_DIR`.
  `roost-session start` refuses while an old-path session answers,
  before taking the locks.
- **The bridge fallback is a restricted proxy.** It forwards only
  `session.identify` and `session.stop`, decoded as the session's own
  envelope, so it can't attach to an old-path daemon. An older laptop
  that reaches an older daemon through a newer remote `roost-session`
  is refused past identify. So upgrade the laptop together with the
  remote.
- **The override is honoured on macOS too** (the harness needs it), and
  it names the socket directory itself.
- **`<ns>` and `run/` are checked one level at a time**, through
  directory fds. `<ns>` must be owned and not group/world-writable,
  since it is also the log dir. `run/` must be exactly 0700.
- **The home's ancestors (`~/.local`, `~/.local/state`) are the user's
  own**, so they aren't checked.
- **A non-UTF-8 socket path is refused**, as is one over 107 bytes on
  Linux or 103 on macOS.
- **The uploads fallback stays in `/tmp/<ns>-<uid>/files`.**
- **`e2e-old-session` has no ssh half:** it drives a local session, in
  Linux CI and by hand. Its pinned releases must predate M2. #652 retires the probe and the bridge fallback one release later.

#### The path

On Linux, the session socket path is resolved as follows:
1. **`ROOST_SESSION_SOCKET_DIR`, if set.** This is a new, explicit
   override for tests and unusual setups.
2. **Otherwise `<home>/.local/state/roost-session/run/roost.sock`.**
   `<home>` is the **passwd home directory** (`getpwuid_r`), not
   `$HOME` or `$XDG_STATE_HOME`. Those can differ between a desktop
   login (`environment.d`), an SSH login and `sudo`, which is exactly
   the bug G1 is about.

Further details:
- Debug builds use `roost-session-dev`.
- `XDG_RUNTIME_DIR` no longer affects the **session** socket. The UI
  socket (`roost` / `roost-iced` profiles) keeps its XDG runtime path,
  because a UI always runs in a desktop login.
- The session's state and logs keep their current XDG paths.
- **`ROOST_STATE_DIR` keeps its documented meaning:** it moves
  persistent state only, never a socket (`paths.rs:169-174`,
  `derived_session_state_dir` at `paths.rs:411-423`,
  `docs/reference/paths.md:33-35`). Test isolation of the socket uses
  `ROOST_SESSION_SOCKET_DIR`.
- **macOS is unchanged** (`~/Library/Caches/RoostSession/roost.sock`,
  already under the home directory).

#### Why the home directory, not `/run/user` (Rex's choice)

**Why `/run/user` doesn't fit Roost:**
- Remote clients find the session by running `client-bridge` in the SSH
  login context, and Tailscale SSH doesn't set `XDG_RUNTIME_DIR` there.
- Logout deletes `/run/user/<uid>` unless linger is enabled, which is a
  system setting.
- The daemon is meant to outlive logins, without a service.

**Why Rex can use it:**
- Its remote clients reach a listener the server owns.
- Its login server opens real logind sessions.
- Its personal server is meant to be short-lived.

Rex itself falls back to `~/.local/state/rex/run` when `/run/user` is
missing.

**Out of scope:**
- **Shared or NFS home directories** (decided 2026-10-28).
- **Encrypted homes** (ecryptfs, systemd-homed) that unmount at logout.

#### Creating and validating the directory

**What the current code does:**
- `validate_runtime_dir` rejects any symlinked component and any
  group- or world-writable ancestor without the sticky bit
  (`crates/roost-ipc/src/runtime_dir.rs:57-60, 116-124`).
- `create_leaf` is not recursive (`runtime_dir.rs:134-137`).
- `prepare_directories` validates the socket directory before creating
  the log directory (`crates/roost-session/src/start.rs:197-210`).

**Why it breaks under the home directory:**
- Home-directory setups break the ancestor and symlink rules:
  - Fedora Atomic, where `/home` is a symlink to `/var/home`;
  - dotfile managers that symlink `~/.local/state`;
  - `~/.local` created group-writable under umask 002.
- A fresh home directory has no `~/.local/state` at all.

**Required:**
- Canonicalize the parent path first.
- Create `roost-session/` and `run/` at mode 0700 if they're missing,
  before any validation.
- Validate only the directories Roost owns: owned by the uid, mode
  0700, not a symlink.
- Every rejection is an error that names the path and
  `ROOST_SESSION_SOCKET_DIR`.

#### Path length

Check at resolve time against the Unix socket limit: 108 bytes
including the terminating NUL on Linux, 104 on macOS. A path that's
too long is an error naming the path and the override. Never fall back
silently.

Test scratch directories are the usual cause. A scratch base plus
`/roost-session-dev/run/roost.sock` adds about 33 bytes.

#### Stale sockets

The home directory, unlike `/run/user`, survives reboots. The existing
stale-socket detection (`socket_state`) must handle a socket file left
over from a previous boot. Add a test.

#### Finding a daemon on an old path

This step exists only for the upgrade. Rules:
- **When to probe:** only when nothing answers at the new path.
- **Where:** `$XDG_RUNTIME_DIR/roost-session[-dev]/roost.sock`,
  `/run/user/<uid>/roost-session[-dev]/roost.sock`, and
  `/tmp/roost-session[-dev]-<uid>/roost.sock`.
- **Generation-independent:** don't assume the old daemon speaks
  protocol 6; check what `session.identify` returns.
- **Connect only to identify and stop, never to attach.** Report it as
  a session on an old path, and offer the existing restart flow: stop
  it over its socket, then start at the new path.
- **An honest outcome when nothing is found:** if the state lock is
  held but no candidate path answers, report "a session holds the state
  lock; its socket wasn't found", with the lock holder's pid.
- **Bridge fallback:** `client-bridge` also falls back to the old paths
  when the new one is missing. Otherwise a laptop couldn't stop an old
  remote daemon, the needs-restart check couldn't run, and an old
  daemon would leave a laptop showing "no session".

Where it applies:
- `session start` refused by the lock;
- `roostctl session status`;
- `roostctl doctor` ("a session is running at a path this build
  doesn't use");
- the `localhost` target (`crates/roost-ipc/src/ssh.rs`);
- `client-bridge` (`crates/roost-session/src/bridge.rs`);
- the bootstrap's `start`.

Removing the old-path probe one release later is tracked as an issue,
not a TODO.

#### What must change

**Code and docs:**
- **`resolve_paths_linux` and its golden tests.** It's documented as
  "byte-stable", so this is a deliberate move.
- **Path wording:**
  - the module header in `paths.rs` (lines 13-38, the "two locks"
    narrative);
  - the `socket_lock_path` doc;
  - `session_launch.rs:615-616`;
  - `single_instance.rs:22`;
  - `docs/reference/architecture.md:129` ("undiscoverable by
    construction").

**Test harness:**
- `tools/roosttest/session.py` (`make_env`, and its own path
  construction around lines 26, 107 and 824-890);
- `tools/roosttest/test_host_ssh.py` (`FAKE_SSH_SESSION_ENV`; SSH
  forwards no environment, so the far side isolates through
  `ROOST_SESSION_SOCKET_DIR` set by the fake SSH, or through the debug
  namespace);
- `tools/roosttest/test_host_bootstrap.py:380` (it hardcodes the `/tmp`
  fallback);
- `tools/roosttest/agent_jail.py`, `test_agent_hooks.py` and
  `test_ssh_transport.py`;
- `tools/roosttest_unit/test_socket_paths.py`;
- `crates/roost-session/tests/{identify_test,agent_hook_test,client_bridge_test}.rs`;
- `tools/session/live/lib.sh:340-440`, which is built entirely around
  the remote login's `XDG_RUNTIME_DIR` and requires linger.

**CI cells that embed the private-`XDG_RUNTIME_DIR` trick:**
`.github/workflows/ci.yml` around 951, 974 and 1199.

**Lanes to change:**
- **`e2e-old-session`** (`tools/roosttest/test_old_session.py`). A
  released daemon binds the old path, and this build will report it
  rather than connect. Rewrite the local half to assert
  "reported → restarted → reachable at the new path". The SSH half
  (released bridge plus released daemon) stays coherent. Its pinned
  releases predate protocol 7, so after M1 the lane covers only the
  forced update until the next release.
- **`e2e-host-local-spawn`, `e2e-host-client`, `e2e-host-localhost`,
  `e2e-local-backend` and `e2e-session`:** move them to
  `ROOST_SESSION_SOCKET_DIR`.

**Docs:**
- `docs/reference/paths.md` (the session path table);
- `docs/development/setup.md:73-77`;
- `docs/getting-started/first-run.md`;
- `docs/guides/host-sessions.md:112` (`echo
  "$XDG_RUNTIME_DIR/roost-session/roost.sock"`);
- `CLAUDE.md` (Architecture: the socket paths).

#### Acceptance

**The core case:** on Linux, a daemon started from an environment with
no `XDG_RUNTIME_DIR`, a different `HOME` export and no `XDG_STATE_HOME`
is found by a client that has all three set. End-to-end test.

**Unit and end-to-end tests:**
- a fresh home directory with no `~/.local/state`;
- a symlinked `~/.local/state`;
- umask 002;
- the path-length boundary at the exact byte;
- a stale socket file from a previous boot;
- debug and release namespaces isolated from each other;
- the override agreeing between launcher and client;
- an old-path daemon being reported (not attached), then restarted,
  with state kept;
- a lock holder with no findable socket;
- two starts at the same time;
- the bridge's old-path fallback;
- a lookup failure that must not start a daemon.

**Done when:** goldens are updated and every lane and CI cell above is
green.

---

### M3. G2: tab size ownership

**Status: shipped on the branch (plan 084); PR pending.**

This overturns plan 057 R15, "last interactor sets geometry"
(`tab_task.rs:406-411`), and roadmap D7, "detach never resizes back"
(`attach.rs:209-210`). Both are pinned by tests that M3 rewrites,
including `tools/roosttest/test_session_attach.py` and
`crates/roost-engine/tests/attach_forwarder_test.rs`. The decision-log
entry in `docs/development/vision.md` is
[DL-31](../docs/development/vision.md#dl-31-a-tab-has-one-size-owner-2026-10-30);
it names both.

*How it shipped (plan 084 corrections to this section). The rule text
below is already corrected where an entry changes it:*

**Terms**
- **The wire says `client_id`.** It is the doc's "instance id": opaque,
  1-64 bytes of `[A-Za-z0-9._-]`, minted once per iced process. The
  label is 1-64 characters, display only.
- **An owner goes on the wire as `{client_id, label}`.** `ResponseError`
  has no data field, so `owned` names the label in its message only.
- **`tab.resized` is live-only.** No geometry or owner rides the wire
  `Tab`, and a reconnecting client learns a tab's owner from the attach
  reply and from `tab.resized`.
- **The data plane is `ROOSTDP3`.** Protocol 7 is amended in place; there
  is no generation bump.

**The rules**
- **A `take: true` attach takes provisionally (rule 10, accepted by
  Charlie).** The resize must happen before the snapshot and other
  clients must see it in band, so an attach that later fails or is
  cancelled may have been announced as owner (GRID, `tab.resized`) for
  that window. It never ends up owning: it is never committed, never a
  handoff candidate, and its take is rolled back to what it replaced,
  when that is still there, else handed on. The cost is a possible
  second reflow.
- **Rollback.** A predecessor that committed meanwhile comes back live;
  one that is gone defers to its own predecessor; an unowned one runs
  the handoff. A failed resize-back keeps the predecessor and logs.
- **A taking attach whose terminal resize fails is accepted as a
  watcher.** A child that never takes the winsize gets `internal`, and
  the attach rolls back.
- **Timings.** Grace 5 s, scroll settle 300 ms, iced's `DETACH` drain at
  most 200 ms.
- **`DETACH` is a data-plane frame.** Rules 6 and 7 need the difference
  between a deliberate detach (an immediate handoff) and a disconnect
  (grace), and the wire had none. Before the accepted line it cancels
  the attach. It is terminal: the reader stops, and a frame already
  buffered behind it is a `protocol-error`.
- **Handoff.** Another attachment of the leaving owner's own client
  comes first, so a reconnect whose new attach beats the old EOF keeps
  ownership. Recency is the attach and every `RESIZE`, in milliseconds;
  ties go to the earliest attachment. A candidate whose grid the
  terminal refuses is passed over, each tried once in one sorted pass.
- **Release is per client.** `tab.release_size` puts the client in a
  `released` set that every attachment it has, or opens later,
  inherits; only a successful take by that client clears it. Otherwise
  a still-attached owner's release would hand the size straight back.
- **Grace.** The record keeps the PTY's grid, for a cancelled reclaim to
  restore, and the owner's label, for a returning data attach that
  arrives before its client's next hello.
- **GRID consumes no seq.** Its `seq` is the last PTY seq before it.
  It is held with the tee records until READY and filtered by
  `grid_gen` against both snapshot and resume. No GRID follows EXIT, and
  queued PTY output is ingested before any geometry change.
- **GRID and `tab.resized` fire only when (cols, rows, owner id+label)
  change.** A metrics-only change, or ownership moving between two
  attachments of one client at the same grid, is not announced. Owner-only
  changes are.
- **`tab.resized` is generation-checked.** Only the pipeline generation
  the workspace row last registered may publish it, checked under the
  workspace lock with the publish, so a replaced pipeline never announces
  for its replacement.
- **Frames before the accepted line.** The queue holds 64 and the reader
  never stops (so EOF and `DETACH` always cancel). A 65th ordinary frame
  is a `protocol-error` rejection. A malformed frame before commit gets
  the same. The queue is drained after each pre-commit step and once
  more right before the accepted line.
- **After EXIT**, size and owner are frozen. An attach is
  non-provisional and records nothing, `tab.resize` answers ok and
  changes nothing, and `tab.take_size` answers the current grid.
- **`tab.take_size` uses the client's newest committed attachment.** A
  client whose only attachment is mid-attach gets `not-found`; a resize
  failure is `internal`; the hello check runs before the tab lookup.
- **`tab.resize` replies after the decision and the terminal apply**,
  not the winsize ack. Button-less mouse motion never takes (rule 3
  names clicks and the wheel).
- **`owned` is session-scoped.** It reaches a UI socket only through a
  verbatim forward. The facade's code name is `size_owned`.
- **Test-mode knobs and ops:** `ROOST_TEST_OWNER_GRACE_MS`,
  `ROOST_TEST_ATTACH_HOLD_MS`, `ROOST_TEST_OWNER_CLOCK=frozen` (which
  stamps `updated_at` 0), `tab.fail_next_resize`, `tab.drop_data_conns`
  (a bare EOF).

**Client identity**
- **`client.hello` is session-only and must come first.** After any
  other request, or a second time, it is refused `invalid-param`. A
  session-wide registry maps client id to label (last hello wins, dropped
  when no connection of that id is open). It is not a mutating op, so a
  stopping session still answers it. An unknown or missing field answers
  `unknown-field` / `missing-param`, the engine's decode convention.
- **The bridge preamble is acknowledged, not fire-and-forget.** The
  bridge writes it, reads one line back, and on anything but
  `{"bridge":"accepted"}` (a protocol-6 daemon's `parse-error`, a closed
  socket, 2 s of silence) closes, re-dials and relays with no preamble,
  silently. A pre-M3 daemon behind an M3 bridge therefore sees bridged
  connections as local. The restricted old-path proxy never writes it.
  Only `roost-session serve` accepts it; a second preamble, a non-object
  `bridge` or a line over 4 KiB closes the connection.
- **iced ignores any `client.hello` error** (`unknown-op` from protocol 6,
  `not-supported` from M2's old-path proxy); `identify` still decides
  compatibility. A hello that gets no answer is not trusted to leave the
  connection in step: iced dials a fresh control connection and says
  hello there too, because a connection that never said it is refused
  `tab.take_size` for as long as it lasts. Only a second lost hello goes
  on without one, on a third connection.
- **Hello is optional, but only first.** A connection that never says it
  is served everything except the ops that act for the caller's client
  (`tab.take_size`, `tab.release_size`), which refuse it
  `invalid-param`.
- **`roostctl` sends no hello** (a non-goal). `tab.take_size` is
  addressed by tab and the connection's hello'd client id, so
  `roostctl tab take-size` asks the UI, which takes the size for its own
  client.

**The iced side**
- **Attach intent.** A user switch, a pending creation and `roostctl`
  activation take; a close fallback, restore and slot re-arm watch. The
  intent is spent when the accepted JSON line is parsed, so a forced
  re-attach never takes, and an explicit request upgrades an automatic
  attach that is not yet accepted (a re-dial with `take`).
- **`DETACH`** goes out in every live phase with a bounded 200 ms drain,
  and a dial still waiting for its reply sends it right behind the
  handshake line. Closing the window detaches too, waiting out the
  drains from outside the runtime.
- **Input marking.** Keys, IME and paste take before their bytes; clicks
  and the wheel are delivered, then take; hover, focus reports and pointer
  cancel never do. A copy takes after the text is captured, and only when
  this window is not the owner; a copy-on-select that copies nothing does
  not take. A settled scroll takes only if another client owns the tab,
  and only the wheel's local scroll and Page Up/Down arm it, not selection
  autoscroll.
- **`RESIZE` carries FOCUSED**, captured at resize time. A focused resize
  while this window is not the owner sends even at an unchanged grid
  (font change, palette preview); unfocused, an unchanged grid dedupes.
  The window's first layout, and any regrid before it, goes out
  unfocused whatever focus says. It is the window settling: it reports
  the grid that an automatic attach made before it (a restore, or the
  slot's tab at launch) already carried, and as a focused resize it
  would take the owned tab that attach was told to leave alone. A later
  slot re-arm sends no `RESIZE` at all. iced can drop that first resize
  event (a full subscription queue), so a font or sidebar change the user
  makes lays the window out too, and goes focused.
- **The mirror is the PTY's grid.** The window's own grid is kept apart as
  `advisory`. A GRID applies live, to the decoder, or deferred during
  hydration; hydration finishes at the grid the payload ended at. The
  withhold deadline only sends the held `RESIZE` and re-arms; it never
  resizes the decoder. The owner comes from the accepted line and every
  GRID only. A GRID whose seq is not the last PTY seq re-attaches.
- **Resume points.** Detach keeps the resume point when the kept grid
  equals the PTY's or none was heard; the session's `Regridded` refusal
  covers the rest. A resume at exactly a GRID's seq answers `Regridded`,
  so detaching right after a take costs a snapshot.
- **The offset view.** A grid taller than the view is drawn from the
  fewest rows above it that keep the cursor row on screen. The cursor row
  is counted from the live bottom and keeps its last value while the
  cursor is off-screen; a DECTCEM-hidden cursor still anchors. Drawing,
  cell mapping, overshoot, the IME rect, autoscroll and Page Up/Down go
  through it. A scroll that reaches the oldest row pins the view to the
  grid's top; a GRID keeps the pin; a downward scroll or a keystroke
  releases it. When the grid differs from the view, any point in the
  widget clamps to the grid, padding included. In-process tabs are
  unchanged.
- **The marker is a tab-band badge**, "Sized by `<label>`" with a Take size
  button, shown only while another client owns the host tab on screen.
- **Take size** is one function behind the badge, the palette row ("Take
  Size"), the unbound `take_size` keybind action, the UI-socket
  `tab.take_size` and `roostctl tab take-size --tab`. The UI op resolves
  the tab like other tab ops and returns the session's result. An
  in-process tab answers its own grid `{cols, rows}`, not `{}`: one result
  shape, since an in-process tab always follows the window. A missing one
  is `not-found`. `tab.dispatch_mouse_event` accepts a host tab ref, so the
  host-client lane can drive pointer input.
- **The background wave** (rule 8). The refusal is final: the wave parks
  the tab, stamped with the owner epoch the mirror folds from
  `tab.resized` and a mirror generation (a reset or reconnect bumps it),
  and judges a refusal by the epoch and generation of its own send. Only a
  tab this window parked waves again on going unowned; an unowned
  `tab.resized` at another grid just forgets `last_sent`, so two windows
  never chase each other's resizes. A retryable error always retries with
  backoff. Parking is cleared on reconnect, resync and rebuild.
- **Test readouts** (test mode only): `app.window_metrics` gains
  `grid_offset_rows`, `mirror_grid` and `size_owner`; `host.size_trace
  {tab_id}` returns `{actions, wave_sends, parked}`, recorded when a frame
  is queued on the main thread, not when it is written.

#### Terms

- **Client:** an instance of a Roost client: one iced window, `roostctl`
  process or phone app. It has:
  - an opaque **client id** (`client_id` on the wire; first called the
    instance id), stable across reconnects for the client's lifetime;
  - a display **label** (iced sends the hostname, capped at 64
    characters), used only for display.
- **Attachment:** one data connection from a client to one tab.

#### The rules

1. **One owner at most.** Each tab has zero or one owning attachment.
   The owner's grid is the PTY's size.
2. **Attaching.** The attach handshake carries an explicit `take`
   intent.
   - **An explicit tab switch takes ownership.** That's selecting the
     tab from the sidebar, the tab strip, a keybind, the palette or a
     `roostctl` activation. The client sends `take: true`. The PTY
     resizes to the client before the snapshot is encoded, as a
     `focus: true` attach does today.
   - **An automatic attach** sends `take: false` and never takes an
     owned tab. That covers restoring the selected tab at launch,
     re-attaching after a reconnect, lag or resume, and hydration.
     Its grid is recorded as *advisory*, and the client is shown the
     owner's size.
   - **An unowned tab** is taken by any attach, because nobody else
     is sizing it.
3. **Deliberate interaction takes ownership.** Each kind is ordered so
   it acts on the layout the user saw.
   - **Keys and paste:** the PTY resizes to the client's grid, and the
     input is delivered only after the resize has been *applied*.
   - **Mouse events reported to the program** (clicks, wheel): the
     event is delivered **first**, at the current size and encoded
     against the owner's grid through the view offset. Then ownership
     transfers and the PTY resizes.

     *Why this order:* resizing first would make the program redraw,
     then read the click's coordinates against a layout that no longer
     exists. In a permission menu, a click on one option could land on
     another.
   - **Copying a selection** (Roost's own, client-side selection):
     ownership transfers **after** the text is captured, so the reflow
     can't change what was selected.
   - **Scrolling scrollback locally:** ownership transfers once
     scrolling **settles**, 300 ms after the last scroll. The
     viewport stays anchored on the same line across the reflow.
   - **Resizing the window while it has focus:** see rule 4.
   - **If applying the geometry fails,** ownership doesn't transfer,
     any pending input is still delivered at the current size, and the
     failure is logged.
   - **Never:** automatic replies (DA, DSR, color queries), focus
     reports (CSI I/O), and simply being attached and showing the tab.
   - **On the wire:** the client marks deliberate input on the `INPUT`
     frame, and sends a take after a copy or a settled scroll. Both
     are data-plane changes, under protocol 7.
4. **Window resizes.**
   - **The owner's** apply immediately.
   - **A non-owner's, while its window has focus,** is a take: the PTY
     resizes to the new grid. That covers a user dragging the window,
     and a phone's soft keyboard appearing.
   - **A non-owner's while unfocused** only updates its advisory grid.
     Unfocused resizes include a display waking, a monitor change,
     re-tiling while the user is elsewhere, and a font change applied
     in the background. Without this rule, a desk display waking from
     sleep would take the size from the laptop being typed on.
5. **Take size.** An explicit transfer without typing.
   - It's a **session-side op**, addressed by tab and the caller's
     instance id.
   - The UI socket mirrors it as a UI op, so `roostctl`, the palette, a
     keybind and a click on the "sized by X" marker all use the same
     path.
6. **The owner leaves.** That is a detach (including switching tabs),
   a disconnect, or an explicit **release**.
   - **Release is a new op** that mobile sends when backgrounding.
     Losing focus on the desktop does *not* release.
   - A **deliberate detach** (the `DETACH` frame) or a release hands
     ownership on at once. **A disconnect** with no other attachment of
     the same client starts rule 7's grace instead.
   - Ownership passes first to another attachment of the leaving owner's
     own client, else to the attachment with the most recently updated
     advisory grid (the attach and every `RESIZE`). Ties go to the
     earliest attachment.
   - A release excludes the releasing client until it takes again.
   - If no attachment remains, the tab keeps its size and has no owner.
7. **Reconnecting owners.** An owner that reconnects with the same
   client id within a **5 s** grace period gets ownership back. That covers lag,
   network blips and resume. There's no handoff churn and no double
   reflow.
8. **Background resizes.** These are control-plane `tab.resize` calls:
   iced's background wave and `roostctl tab resize`.
   - They apply only to tabs with no owner.
   - On an owned tab they're refused with an explicit `owned` error
     naming the owner's label in its message. `tab.resize` waits for the
     tab task's decision and terminal apply before replying (not the
     winsize ack). It used to return once the request was queued.
   - The background wave treats `owned` as a final answer: it parks the
     tab and doesn't retry it. When a `tab.resized` says the tab has
     no owner, the wave starts again, for a tab this window parked.
9. **`tab.write`** (the compose box, quick replies, `roostctl send`)
   never changes ownership or size.
10. **One serializer.** Ownership state, advisory grids, and every
    attach, detach, input, resize, take and release decision live in
    the **tab task**, which already serializes a tab's PTY input and
    geometry.
    - `TabCmd::Input` and `TabCmd::Resize` gain the attachment and
      instance identity plus the user-initiated flag.
    - Attach and detach become tab-task commands that return the owner
      and grid.
    - The `resize_first` decision at admission
      (`crates/roost-engine/src/ipc.rs:2011`) is removed. Two attaches
      to an unowned tab at the same moment resolve in the tab task.
    - A failed or cancelled attach never **ends up** owner. A `take:
      true` attach is owner provisionally until its accepted line is
      written, and is rolled back if it fails first.
11. **Resizes are ordered with output.**
    - A new server-to-client **data-plane frame**, `GRID`, carries
      `{seq, cols, rows, owner}`. The tab task emits it into the output
      stream at the exact sequence position where a geometry change is
      applied. It consumes no seq: `seq` is the last PTY seq before it.
    - Clients resize their mirror when they reach the frame, so output
      written for the old grid and the new grid is never mixed.
    - The control-plane `tab.resized {tab_id, cols, rows, owner}` event
      still goes to every client, for sidebar and UI state only.
      `mirror.rs` folds it; the attach state machine follows the frame.
12. **Viewport and grid are separate.**
    - **The grid:** a client's mirror always equals the PTY's grid.
    - **The viewport:** what fits in the client's view. Non-owners draw
      the owner's grid inside it, padded or scrollable, with "sized by
      \<label\>" and Take size.
    - **No echo:** owner-size updates never overwrite the client's own
      advisory grid, and never echo back as `RESIZE`.
    - This breaks today in iced's post-swap resize and its
      hydration-time resize hold (`host_tab.rs:599`, `:1133-1143`).
      Both must change.
13. **The attach handshake's `focus` field is replaced** by rule 2's
    `take` intent under protocol 7. iced always sends `focus: true`
    today, even for automatic re-attaches (`host_tab.rs:537`,
    `:1495`), so iced must learn to tell explicit switches from
    automatic attaches.

#### Client identity and the bridge preamble

These are used here, and by M4.

- **`client.hello`, a new session-socket op.** It's the first request
  on a control connection, and carries the client id and label. The
  data-plane attach handshake carries the client id too.
  - All of this is session-only and rides protocol 7.
  - Session requests are `deny_unknown_fields`
    (`messages.rs:2618-2623`), so these are new message types, not new
    fields on old ones.
- **The bridge preamble.**
  - **The problem:** `client-bridge` is a pure byte pump
    (`bridge.rs:9-18`), so the session can't tell bridged connections
    from local ones.
  - **The change:** before relaying, the bridge writes **one preamble
    line toward the daemon**:

    ```json
    {"bridge":{"ssh_connection":"…","ssh_client":"…"}}
    ```

    The values come from the bridge's own environment, which is the
    laptop's SSH connection.
  - **Server side:** the IPC server's first-line classifier
    (`crates/roost-ipc/src/server.rs`, around 698 and 902) accepts,
    acknowledges and strips the preamble before the attach handshake or
    the first request, on both planes.
  - **What stays pure:** the direction toward the client (stdout).
  - **No preamble:** the connection is local. That includes a bridge
    whose daemon never acknowledged one.
  - **Trust:** the only callers are same-uid processes
    (`serve.rs:369-375`), so the flag is advisory, not a security
    boundary.
- **Labels are for display only.** Two windows on one host share a
  hostname, and the client id tells them apart.
- `#476` (a client list) can grow from this later. It's not in scope.

#### The iced side

- attach per rule 2, and user-input marking per rule 3;
- following the frame (rule 11);
- the non-owner view (rule 12), with mouse and selection coordinates
  mapped through the offset;
- the marker and Take size;
- the background wave changes (rule 8).

In-process tabs have one client and don't change.

#### Acceptance

End-to-end tests with two clients on one tab, through
`tools/roosttest`, cover:
- an attached client that only shows the tab doesn't resize it;
- an automatic re-attach doesn't take ownership;
- an explicit tab switch takes it before the snapshot;
- keys and paste take it, with the resize applied before the input;
- a click and a wheel event are delivered at the old size, then the
  tab is taken;
- a copy takes it after the text is captured;
- a settled scroll takes it, keeping the viewport anchored on the same
  line;
- a focused window resize takes it, and an unfocused one only updates
  the advisory grid;
- focus reports and automatic replies don't take it;
- `tab.write` changes nothing;
- a failed geometry apply;
- two clients attaching at the same moment;
- a cancelled attach;
- a disconnecting owner handing off to the newest advisory grid, with
  ties going to the earliest attachment;
- release;
- a reconnecting owner reclaiming within the grace period;
- a background resize on an owned tab refused with `owned`, and the
  wave not retrying;
- Take size;
- the frame arriving at the right sequence position, checked by
  delaying and reordering delivery between the event and data
  connections;
- `tab.resized` reaching every client.

Also: hydration and resume races, and mouse coordinates in the
non-owner view. A screenshot check supplements these tests but can't
prove ordering.

---

### M4. G9: tab environment by opener

#### The rule: a tab's environment reflects where it was opened from

The PTY's environment is **built explicitly for each tab**. It is not
inherited and then trimmed, and nothing is mutated in the daemon's own
process: `remove_var` is unsafe once threads exist, and it doesn't
show up in `/proc`. Today `build_command` starts from the daemon's
environment (`pty.rs:2380-2483`); M4 changes that.

**The variable set.** The *opener set* is:
- `DISPLAY`, `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`,
  `SSH_AUTH_SOCK`;
- `SSH_CONNECTION`, `SSH_CLIENT`, `SSH_TTY`.

`XDG_RUNTIME_DIR` is **not** in it. It belongs to the machine and user,
not to the opener: every Linux tab gets
`XDG_RUNTIME_DIR=/run/user/<uid>` when that directory exists and is
owned by the uid, whoever opened it. That keeps `systemctl --user` and
podman working in remote tabs.

1. **Baseline.** Each tab starts from the daemon's environment with
   **the whole opener set removed**. That holds whether a desk or an
   SSH connection started the daemon.
2. **Local openers** are connections with no bridge preamble: the
   desk's iced, or `roostctl` run on the machine.
   - The opener sends name→value pairs for the opener set, minus the
     `SSH_*` three. The server applies only allowlisted names and
     rejects any other key.
   - Values cross the socket because the session can't read a peer's
     environment. Same-uid only.
   - An opener-set variable the opener doesn't have stays unset.
   - Tools then behave as local: GUI apps open on that machine's
     display, and `git push` uses the local agent.
3. **Remote openers** are connections with a bridge preamble (M3).
   - The tab gets `SSH_CONNECTION` and `SSH_CLIENT` from the preamble,
     and none of the rest of the opener set.
   - Tools behave as remote. The desk's display and agent are
     unreachable.
   - **Why the bridge doesn't forward its `SSH_AUTH_SOCK`:** a
     forwarded agent socket does exist on this host, but it dies with
     that SSH connection, so it's wrong for a long-lived tab.
4. **Forwarded ops.** When iced forwards a request from its UI socket
   to the session (DL-20), for example `roostctl tab open` hitting
   iced, it forwards the original caller's environment and origin, not
   its own. If the original caller reached iced's socket over SSH, the
   caller declares that.
5. **Restored tabs.**
   - Each tab's opener class and its applied opener-set values are
     persisted in `state.json`.
   - After a restart, including the upgrade restart M2 forces, restored
     local tabs get their saved values. A client that triggers the
     start or restart may refresh them with its own current values.
   - Restored remote tabs get their saved `SSH_*`.
   - Tabs restored with no saved class get the baseline.
6. **macOS.** The same rules. A desk-started Roost-Iced has launchd's
   `SSH_AUTH_SOCK` in its environment and passes it as a local opener.
   The daemon's baseline never keeps one.
7. **Frozen Swift.** `env` on `tab.open` goes only to session targets,
   per the release rule. `roostctl tab open` against `Roost.app` keeps
   working unchanged.

**Every op that spawns a PTY is in scope.** The M4 plan writes the full
inventory: `tab.open`, project creation's first tab, startup restore,
backend migration and anything else that spawns.

#### Why per-opener, not "latest attach" (tmux's `update-environment`)

- A process's environment is fixed when it starts, so only new tabs
  can follow a client at all.
- A daemon-wide "latest attach" environment flips as clients come and
  go. A tab opened at the desk could get the laptop's environment just
  because the laptop attached later.

#### Clipboard (checked against Claude Code 2.1.312)

- Claude's copy writes to the native clipboard (`pbcopy`, `wl-copy`)
  **and also emits OSC 52**. It skips OSC 52 only on VTE-based
  terminals or terminals that print unknown sequences, and Roost is
  neither.
- Roost forwards OSC 52 to the clients viewing the tab.
- So a copy reaches whoever is viewing, whichever rule the tab was
  opened under.
- The environment mainly decides whether the agent opens a browser, and
  where.
- Paste is unaffected: Roost's paste and drop path (`session.put_file`
  plus a pasted path) reads no environment.

#### What was seen on the mac-mini

An SSH-started process there runs in launchd's `Background` context.
Even so, `pbcopy`, `open` and keychain access all worked. Only
`SSH_AUTH_SOCK` was missing.

#### Acceptance

Assert exact environments by capturing the tab process's own
environment (`/proc/<pid>/environ` on Linux, `ps eww` on macOS), not
through a shell, whose startup files would obscure the result.

End-to-end tests:
- a real `client-bridge` run, not a hand-set flag: the remote tab gets
  `SSH_*` from the preamble and none of the rest;
- a local open with values;
- an opener without some variables;
- a non-allowlisted key rejected;
- simultaneous local and remote opens;
- a request forwarded through iced, keeping the original caller;
- restore after a restart;
- `XDG_RUNTIME_DIR` derived for both kinds of tab;
- both daemon start contexts: desk-started (its baseline has a display
  that remote tabs must not get) and SSH-started;
- `roostctl tab open` against `Roost.app`.

Named smoke tests, by hand:
- **Linux:** a desk-opened tab launches a GUI app; a laptop-opened tab
  reports SSH and has no display variables.
- **The mini:** a local-opened tab has launchd's `SSH_AUTH_SOCK`.

---

### M5. B1: host-client modules into `roost-client`

**What moves** from `crates/roost-iced/src/host_conn/` into a new
`crates/roost-client` crate:
- `state.rs`, `mirror.rs`, `reconnect.rs` and `queue.rs`;
- their supporting pieces:
  - `upload.rs`, because `queue.rs` imports `super::upload`
    (`queue.rs:28`);
  - `ConnectFailure`, because `reconnect.rs` imports it from
    `host_conn.rs:169`;
- the inline tests, which move with the modules.

**What else is allowed:** `pub(crate)` → `pub` visibility changes, and
minimal supporting-type extraction. No logic changes.

**Timing:** after M3, which edits `mirror.rs`. It lands on `main`
because these files churn (#568, #570, #597).

**Acceptance:**
- **iced uses the moved code:** it imports these modules from
  `roost-client`, and no copies remain.
- **No logic changes:** `git diff -M --stat` shows only renames plus
  visibility and `use` changes.
- **Clean dependencies:** `roost-client` builds on its own and doesn't
  depend on `roost-iced`, iced or winit. `cargo metadata` checks this,
  the same way the `examples/ipc-consumer` check does.
- **Gates green:** `make test-rust`, `make check-iced`, `make e2e-iced`
  and the host lanes. Every desktop user's local tabs run on this code
  under `local-backend = session`.

---

## Test plan, across milestones

- **Unit tests** live inline, in `#[cfg(test)]`:
  - path resolution and directory creation;
  - the ownership state machine;
  - building the environment;
  - the 7501 composition, mapping and precedence.
- **Functional end-to-end tests** go in `tools/roosttest`.
  - Every new module is added to the Makefile lane lists and to the CI
    step lists. `tools/roosttest_unit/test_e2e_lists.py` enforces both.
  - Session behaviour goes in the session lanes; iced behaviour in
    `ICED_E2E_TESTS`.
- **Byte-level OSC tests** use test mode: `ROOST_TEST_MODE=1`,
  `tab.feed_pty_bytes` and `tab.capture_pty_input`.
- **Every new test gets a negative control.** Break the code it pins
  and confirm it fails. Undo the break with the inverse edit, never
  with `git checkout`, which can discard other uncommitted work.
- **Tests that existing behaviour pins get rewritten, not supplemented,**
  where this release changes the rule:
  - R15 and D7 (M3);
  - path goldens (M2);
  - pin-exact strings (M1).
- **Gates:** see Release rules.

## Docs this release changes

These change in the milestone PRs, not here:
- `docs/reference/ipc.md`: protocol 7, `client.hello`, the bridge
  preamble, the grid frame, `tab.resized`, ownership, take and release,
  program status, memory, `tab.open` `env`, the retired `focus` field.
- `docs/reference/ipc-compatibility.md`.
- `docs/reference/paths.md`.
- `docs/reference/architecture.md`.
- `docs/getting-started/first-run.md`.
- `docs/guides/host-sessions.md`: the "a client that only watches"
  sentence becomes true; environment; the socket example.
- `docs/development/host-sessions.md`.
- `docs/development/setup.md`.
- `docs/development/vision.md` decision log:
  - ownership, replacing R15 and D7;
  - the session socket path;
  - environment by opener;
  - OSC 7501 precedence.
- The Zig 0.17 docs.
- `CLAUDE.md`.
- `CHANGELOG.md`, with one protocol bullet.

## Out of scope (tracked)

- **Kitty graphics:** #645.
- **Scrollback search:** #167.
- **History compression and parking:** #366, measured with M1's memory
  query.
- **Child program-status records as Agents-panel rows:** follow-up
  issue, filed in M1.
- **Per-host auto-connect, a shipped user service, and package-versus-
  bootstrap binaries** (sessions-everywhere G3/G4). Decided 2026-10-27:
  no system or user service is required.
- **Flipping the local-backend default** (#480, G6): after this
  release.
- **Rex-style local-only reflow on a client:** a later experiment,
  mainly for mobile.
- **The mobile client:** its own branch after the release.
