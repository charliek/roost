# Removing in-process local tabs (discovery)

**Status: discovery, 2026-11-14. Decided, not scheduled.** Charlie decided
to remove the in-process local backend entirely, in one release, and then
deferred that release: the next release is the agent work (Phase 3a,
#261, #623, the gx removal and the #719 follow-ups). This note records the
decision, what the removal involves, what it found that matters before
then, and the decisions still open. File and line references are as of
`main` @ `e49d9d2d` unless marked otherwise. Things inferred and not run
are marked **(inference)**.

It replaces the "Retiring in-process local tabs (later)" section of
[`sessions-everywhere.md`](sessions-everywhere.md) and the default-flip
plan of [#480](https://github.com/charliek/roost/issues/480).

## Decided (Charlie, 2026-11-14)

- **In-process local tabs go away.** Every local tab runs on the
  `roost-session` the app manages. Charlie kept in-process to test the
  session work against; sessions are now all he uses, and the goal is to
  delete the special cases the two backends need.
- **In one go.** No flip-first release (#480's plan: move existing setups
  to `session` by default, keep in-process as a choice) and no
  `--ephemeral` in-process mode (the original HS-5 escape hatch). Both were
  considered and dropped.
- **Not in the next release.** The scope below is too large to ride with
  the agent work. When it is scheduled, it is its own PR.
- **What survives:** a one-way move of an existing in-process layout onto
  the session at upgrade. Projects and tabs reopen as fresh shells, as the
  forward switch does today.

When the removal lands it needs a decision record that supersedes DL-27
("the local backend is a user's choice, not a default flip") and amends
DL-4, DL-17 and DL-24 where they describe the in-process path, and HS-5 in
[`host-sessions-roadmap.md`](host-sessions-roadmap.md) is then done. #480
should be closed by that PR, not before: until then the shipped default
for existing setups is still `in-process`.

## Why it is large

Three things make it XL as one PR, roughly in equal parts.

**1. The product code (deletion, mostly mechanical).**

- About 4.0–4.6k production lines and 3.5–4.5k test lines go, across about
  40 Rust files in five crates (`roost-iced` about 20, `roost-ui-model`
  about 7, `roost-engine` about 4 plus 3 integration tests, `roost-ipc`
  about 5, `roost-cli` about 6).
- The biggest piece is the switch in
  `crates/roost-iced/src/app/local_backend.rs`: the journal, six phases,
  rollback, fence and busy gate (lines 813–3130, about 2,300 production
  lines) and its tests (`switch_tests`, 4308–5756, 31 tests).
- `LocalBackendMode` has about 240 references (111 production) in 18
  files, and goes entirely. `HostId::LOCAL` has 183 (27 production) and
  survives only as a placeholder for a host that is not connected
  (`servicing.rs:1234-1241`, `app.rs:10761`) unless it becomes
  `Option<HostId>`.
- `TabBackend::InProcess`, `TerminalTab::attach`, the in-process hydrate
  and the UI's PTY lifecycle go (`tab_backend.rs`, `terminal_tab.rs:679-691`,
  `app.rs:11445-11495`). `roost-engine/src/session.rs` (`TabSession`, 619
  lines) is used only by `roost-iced` and two engine tests.
- **What stays:** `roost-engine`'s `Workspace`, PTY supervisor,
  persistence and `process::set_tab_env`, because `roost-session` runs on
  them. The UI socket stays too: `roostctl` reaches the window through it
  for the palette, screenshot, `host.*`, `agent set`, `tab focus` and the
  test-mode ops (`docs/reference/cli.md:65-80`,
  `crates/roost-cli/src/window.rs:1-12`). A tab's `ROOST_SOCKET` already
  names the session socket (`crates/roost-session/src/serve.rs:316-320`).
- **The UI-side agent retire stays** as the notice path. The session runs
  the real retire but cannot show a toast; a deferred retire on the UI
  still returns what was unannounced
  (`crates/roost-agent-install/src/retire.rs:332-349`). Only its
  `defer_retire` argument becomes constant.

**2. The tests and CI (as large as the product code).**

- **Every app-driving E2E lane runs in-process today, by a pin, not the
  default.** `tools/roosttest/fixtures/launcher.conf:19-33` writes
  `local-backend = in-process` into every harness-owned UI (copied in by
  `ui.py:804`; `test_smoke.py:89` asserts it), and `Makefile:175` says
  every other lane is pinned to in-process. Other pins:
  `test_boot_failure.py:116`, `test_quit_with_foreground_job.py:121`,
  `real_input_ui.py:43/334`, the two Linux clipboard checks and
  `tools/screenshot/lib.sh:83-114`. 54 of the 61 shared-UI modules are
  in-process.
- **Test bytes must move to the session socket.** About 100
  `tab.feed_pty_bytes` / `tab.capture_pty_input` calls in 22 modules go
  through the UI socket. On a session tab the UI answers those itself
  (`crates/roost-ipc/src/local_route.rs:231-262`), writing into its
  mirror terminal, which does not scan OSC and discards query replies
  (`crates/roost-iced/src/app/tab_backend.rs:226-231, 291-312`). Such tests
  would pass without proving anything. The session socket serves the
  honest ops in test mode (`crates/roost-engine/src/ipc.rs:1717-1719`), and
  the host lanes already use it (`test_host_client.py:594` on). The
  riskiest modules are the OSC, reply and Kitty ones (osc_pipeline,
  mouse_tracking, program_status, kitty pixels and images,
  device_queries).
- **Green-by-skip:** `test_palette.py:88` skips whenever the sidebar has
  sections, which under session it always does, and the in-process half
  of `test_program_status` skips (`:123`). They must be rewritten, not
  left skipping.
- **`test_local_backend.py`** (75 tests, 4,176 lines): about 24 switch and
  journal tests are deleted, the 3 launch-migration tests
  (`:2679-2852`) become the upgrade proof, about 48 session-mode tests
  stay as the local-session lane.
- **Rust:** `attach_test_terminal` (a real `cat` PTY,
  `terminal_tab.rs:381`) has 20 call sites, one of them the
  `attached_test_terminal` wrapper that 30 `interactions.rs` tests call
  (about 49 tests in all), plus 4 direct `TabBackend::in_process`
  constructions in tests, and `make perf-refresh` runs
  through it (`perf_bench.rs:11-12`). All 8 `tab_backend.rs` tests are
  in-process.
- **Lanes:** 6 Make targets and 6 CI steps go or merge (localhost,
  local-spawn, missing-daemon, each with a `-ci` twin);
  `e2e-local-backend` becomes the local-session lane. The `iced-release`
  and wayland-drag jobs and the `e2e-iced` Make recipes do not build
  `roost-session` today (`ci.yml:2106, 2363`; `Makefile:288, 295, 417`).
- **macOS:** no session-mode UI lane has ever run in CI on macOS (#390);
  every session-mode module skips Darwin. The removal would be the first.
- **Teardown:** every owned UI now leaves a daemon running, and
  `_remove_session_state` raises while a session's lock is held
  (`ui.py:974-1023`). A missed stop becomes a teardown failure.

**3. Fixes that stop being optional once there is no fallback.** See the
next section; several already affect session users.

## Found during scoping: worth fixing before the removal

These are bugs or gaps on today's session path. Each helps current
session users on its own and can ship before the removal.

1. **File drop and image paste on local session tabs upload a copy.**
   `target_of` treats a tab as local only when `tab.is_local()`
   (`crates/roost-iced/src/app/file_transfer.rs:299-310`). A session-backed
   local tab is keyed by a host id, so a drop uploads into the session's
   file store and pastes the copy's path, under the remote limits: no
   directories, 10 MiB per file, 256 MiB per drop, and a 512 MiB store
   that never evicts (`docs/guides/host-sessions.md:207-239`). Fix: treat
   the local slot as `Target::Local`, the same filesystem. **(inference:
   affects session users today; not reproduced.)**
2. **A crashed local session is never restarted.** Only the first connect
   attempt may spawn one (`crates/roost-iced/src/host_conn/task.rs:656-657`);
   afterwards the app re-dials a dead socket and shows no card.
3. **The macOS session socket directory check does not resolve symlinks
   first,** unlike Linux after protocol-7's M2 (`paths.rs:471-472`). This is
   the class of failure #520 was filed for (seen in the #719 testing: a
   `/tmp` path is refused because `/tmp` is a symlink on macOS).
4. **`roostctl doctor` has no "can the local session start" checks**
   (binary found, socket directory acceptable, running).
5. **`make build-iced` / `run-iced` don't build `roost-session`,** so a dev
   run can pick up a stale daemon.

## Blockers at removal time

- **The upgrade move.** An in-process user's layout exists only in the
  UI's own `state.json`. Today the forward switch replays it onto the
  session at launch (plan 063 §D5). It can shrink to a one-shot import:
  `retained_migration` (`local_backend.rs:1184-1241`) reads the layout and
  the replay loop (about `2905-3005`) already speaks only `project.create`,
  `tab.open` and `tab.set_title`. Delete each source project from the UI's
  workspace once it lands, so `state.json` is the progress record: a crash
  duplicates at most the project in flight, and no journal, rollback,
  fence, key write or busy gate is needed.
- **The cannot-start card's main button is "Use in-process tabs"**
  (`crates/roost-ui-model/src/notice.rs:227-238`), and a fresh install
  that can't write its config falls back to in-process quietly
  (`local_backend.rs:116-121`). Both need a session-only answer.
- **A partly failed switch keeps the leftover projects in the in-process
  store** (`deletable_sources` keeps incomplete ones,
  `local_backend.rs:1597-1603`), which nothing draws after removal (#631),
  and a re-run would duplicate the tabs that did land **(inference)**.
- **The harness pin** above: the required `e2e-iced` gate runs in-process.

## What users see after the removal

- An in-process user's first launch on the new version: their shells
  already ended when the app restarted for the update; their projects and
  tabs reopen as fresh shells on the session. A one-time notice should
  say that shells now outlive Roost.
- **Quitting Roost no longer ends local shells or agents,** and a UI crash
  no longer kills them.
- **Scripts that subscribe to events on the UI socket break.** The UI
  serves `events.subscribe` only in-process (`ipc.rs:2786-2788`); under a
  session it is `Unsupported` (`local_route.rs:342`). They must use the
  session socket (`identify`'s `local_session_socket`). CHANGELOG and
  `skills/roost/SKILL.md` need a line.
- **Each update that needs a session restart ends running shells** (G4 in
  `sessions-everywhere.md`). A newer app can talk to an older session
  (one-back compatibility), so users choose when to restart.
- **Downgrade:** a v0.1.x build finding no `local-backend` key over an
  emptied `state.json` concludes "existing setup", runs in-process, seeds
  "Untitled 1", and shows the session's work under a `LOCALHOST` band
  (`local_backend.rs:56-65, 91-101`). Nothing is lost.
- Already on session (Charlie): nothing new.

## Prerequisites from sessions-everywhere: status

- **G1, finding the session:** done, #653 (plan 083, `30ce2bc5`). The Linux
  socket is under the passwd home (`paths.rs:975-1000`); a session on an
  old path is reported as needing a restart until #652 retires the check
  at the next protocol bump.
- **G2, window size with several clients:** done, #663 (plan 084,
  `bc8189f1`), DL-31.
- **G9, tab environment:** done, plan 085, DL-32.
- **G5, where session tabs are worse:**
  - Kitty graphics: closed by #677.
  - Bell and clipboard reach every connected client, even for tabs it
    hasn't attached (`event_push.rs:243`); only effects fired while no
    client is connected are lost, by design.
  - Build-skew `vt` attach fidelity: still reduced, only while versions
    differ.
  - Latency: switching tabs measured 15.6 ms in-process against 35.2 ms on
    a session (#566). **Typing latency is still unmeasured**, and there is
    no backend left to fall back to if it is worse.
  - One attached tab per window: still true (#566 open).

## Suggested shape (one PR, each commit reviewed)

- **M0: prerequisites,** safe on today's code: the five items above.
- **M1: the harness on session, product unchanged.** Remove the
  `launcher.conf` pin, make the boot wait session-only, stop the session
  in `end_session`, add a feed/capture helper on the slot's session socket
  and convert the 22 modules, fix the green-by-skip cases, build
  `roost-session` in every lane, re-baseline pixel goldens. Land it green
  while both backends exist, so a failure points at the harness.
- **M2: the one-way import and a session-only boot.** Replace the mode
  ladder and journal resume, settle the partial-import policy (closes
  #631), handle a leftover `switch-journal.json`, retire the key quietly,
  rewrite the cannot-start card, add the one-time notice, rewrite the 3
  launch-migration tests as the upgrade proof with negative controls.
- **M3: delete the in-process backend and the switch in `roost-iced`,**
  and replace or port the Rust test terminal.
- **M4: engine, IPC, model and CLI cleanup.** Delete `TabSession`,
  `InProcessStreams`, `ui_events_subscribe` and the switch gate; remove
  `LocalBackendMode`; check what `roostctl` does with no window running,
  which becomes the normal case ("quit Roost, agents keep running").
- **M5: lanes and macOS.** The local-session lane, the folded host lanes,
  the first macOS session-mode lanes, and on the mac-mini a check that a
  Sparkle update leaves the old daemon running and that Restart Session
  picks the new binary.
- **M6: docs and the decision record** (about 20 files), close #480 and
  #631.

Doing this before the agent work would have shrunk that work (#261 and 3a
would only cover session tabs). Deferring it means the agent work covers
both backends once more.

## Open decisions (recommendation first)

1. **How the import records progress:** consume each source project as it
   lands (recommended); a small progress file; or keep the switch journal
   for upgrades only.
2. **Partial import:** consume the project and log the tabs that failed
   (recommended; closes #631); keep it (re-imported and duplicated on each
   launch); or replay only the missing tabs.
3. **How long the import code lives:** permanently, since it is small
   (recommended); or N releases, after which a user who skipped them lands
   on an empty band with the layout still in `state.json`.
4. **A leftover `local-backend` line:** parse and ignore it, logging once
   if it says `in-process` (recommended). Do not write `session` into the
   config for downgrade safety: `config.conf` has no profile component, so
   a dev build would write the release profile's file
   (`local_backend.rs:85-91`). The import trigger must be per profile
   ("this profile's `state.json` has local projects").
5. **A leftover `switch-journal.json` from v0.1.x:** rename it aside and
   log, or keep the recovery code for one release.
6. **Users who chose in-process explicitly:** move them quietly like
   everyone else, then the one-time notice (recommended); or a confirm
   card first.
7. **The cannot-start card:** "Try again" plus the log path and a
   `roostctl doctor` pointer (recommended), optionally "Copy diagnostics"
   or "Start a fresh session".
8. **A crashed local session:** restart it automatically when the socket is
   gone and no process holds the lock (recommended), without racing
   Restart Session; or show the Start card after N tries.
9. **File drop on local tabs:** paste the real local path (recommended).
10. **Wire compatibility:** keep `identify.local_backend = "session"` and an
    always-set `local_session_socket` for one release, and keep
    `roostctl`'s tolerance for the old switch stream end (recommended); no
    protocol bump for this alone. An older `roostctl` reads a missing
    `local_backend` as in-process (`crates/roost-ipc/src/messages.rs:328-332`).
11. **How test bytes reach session tabs:** a harness helper on the slot's
    session socket (recommended; matches the host lanes); or forward
    bare-id `tab.feed_pty_bytes` and `tab.dump_resolved` from the UI socket.
12. **Rust test terminal:** a `cfg(test)`-only in-process handle (cheap,
    call sites unchanged) or a host-handle fixture over channels (cleaner).
13. **Depth of simplification:** keep `HostId::LOCAL`, `TabKey::local` and
    the UI's own `Workspace` and idle supervisor as placeholders (smallest
    diff), or move to `Option<HostId>`, a separate UI-state type, and drop
    the supervisor from `IpcHandler` (cleaner, more churn). Charlie's stated
    goal is simplification, which argues for the second, perhaps as a
    follow-up commit.
14. **macOS session-mode lanes (#390):** in scope (recommended: with no
    fallback, the macOS lanes must run on sessions), or convert the shared
    lanes and leave #390 open.
15. **The localhost, local-spawn and missing-daemon lanes:** fold into the
    local-session lane; and whether `test_boot_failure` and
    `test_quit_with_foreground_job` are deleted or kept as shell-free and
    daemon-side variants.

## Corrections to earlier notes

- [`sessions-everywhere.md`](sessions-everywhere.md)'s possible order
  (flip the default, then decide on in-process) and its `--ephemeral`
  option are superseded by the decision above.
- That note's G6 says the flip needs G1 and G2 first; both are done.
- The scoping's first pass said the test-mode ops "already work on
  session tabs". Through the UI socket they reach only the mirror (see the
  tests section); only the session socket's ops are a real test.
