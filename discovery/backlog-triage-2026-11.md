# Backlog triage after v0.1.0 (discovery)

Status: **discovery**, a ranked reading of the open backlog, split into separate plans
that can be worked one at a time. It is not a commitment. Written 2026-11-08 and
updated the same day after the GitHub pass below. File and line references are as of
`main` @ `68231b9b`.

Scope: the open issues in `charliek/roost` (84 before the pass, 91 after it), a
feature comparison with Ghostty, WezTerm and herdr, and the local logs on the Pop!_OS
dev box. Three areas have their own notes and are left out of the plans here:
- chrome themes and chrome font size (#584, #164): [`chrome-themes.md`](chrome-themes.md);
- text rendering and fonts (#585, #590, #591, #592): [`text-rendering.md`](text-rendering.md);
- split panes: [`split-panes.md`](split-panes.md).

Agent state and hook delivery belong to [`agent-integration.md`](agent-integration.md).
This note points at its roadmap instead of repeating it.

---

## Summary

- **The GitHub pass is done.** Two issues closed (#570, #388), nine filed
  (#706–#714), 21 enhanced with current findings, eight retitled, and most of the
  backlog relabelled with type, priority and effort. See
  [What changed on GitHub](#what-changed-on-github-in-this-pass).
- **Five plans, worked one at a time:**
  - **A. v0.1.1 quick wins** (M): #134 first, then #697, #714, #712, #711, #713,
    #268, #336, #273, and a test for the closed #570. No rulings needed.
  - **B. Scrollback and find** (L): the Ghostty pin bump, then #707, #167, #169.
  - **C. Agent safety** (M): #706 (confirm close) and #261 (`roostctl` on PATH on
    macOS). The process-identity work (#519, #621) is agent-integration's Phase 3.
  - **D. Everyday actions and the terminal menu** (M): #710, #165, #162, #163.
  - **E. Paths and live config** (M–L): #708, #709.
- **Recommended order: A, then C, then B, D, E.** Agent-integration's Phases 1–2
  can run beside C. See [Order](#order).
- **Ship #134 first.** Each release without background update checks reaches only
  Mac users who click Check for Updates.
- **Several blockers are gone.** The libghostty callback multiplexing that #364,
  #209, #188 and #365 waited on shipped in `2c37afed` (2026-10-28). The search API
  for #167 and prompt-to-prompt scrolling for #169 are at the pin.
- **Plan B starts with a Ghostty pin bump.** The search C ABI changed 279 commits
  after the pin, so a wrapper written against the pin would be rewritten.

---

## How to read this

- **Verified** means checked against the code at `68231b9b`, a pinned header, a log
  line, CI job logs, or `gh`. **Inference** is marked *(inference)*.
- **Effort and complexity:** S is about a day or less. M is a few days, or one small
  plan. L is a multi-commit plan. XL is more than one plan.
- **Impact** weighs how many users hit it and how often, with extra weight for agent
  work, which is what Roost is for.
- **Ruling** means a product decision for Charlie. Per "decisions are not tickets",
  rulings are asked as key questions, not filed as issues.

---

## What changed on GitHub in this pass

All 89 actions succeeded. The inputs and per-action results are kept outside the repo
(the session scratchpad's `gh-actions/`).

### Closed in this pass

- **#570** (stale away-grid on a host attach). Fixed in code by `da1e18e9` (plan 084):
  `HostAttach::note_resize` sets `withheld` only when the geometry differs from
  `advisory`, which holds the handshake grid during the attach
  (`crates/roost-iced/src/app/host_tab.rs:725`, `:746-759`). No test names the case;
  Plan A adds one, and the close comment says to reopen if it fails.
- **#388** (per-host settings convention). A recorded convention with no work. The
  convention lives in `discovery/host-sessions-roadmap.md:1237-1240`.

### Filed in this pass

| Issue | Title (short) | Labels | Plan |
|---|---|---|---|
| #706 | Confirm before closing a tab, project or window running a process or agent | priority/high, effort/medium | C |
| #707 | Configurable scrollback (2,000 lines today) | priority/high, effort/medium | B |
| #708 | Clickable file paths that open in an editor | priority/medium, effort/medium | E |
| #709 | Reload config without restarting | priority/medium, effort/medium | E |
| #710 | Everyday actions: clear, select all, page and top/bottom scroll, move tab | priority/medium, effort/medium | D |
| #711 | Skip no-op tab title and cwd commits | type/chore, priority/low, effort/small | A |
| #712 | Linux: `roost` with no display panics after bootstrap | type/bug, priority/low, effort/small | A |
| #713 | Log noise: `pty resize failed err=NotFound` | type/chore, priority/low, effort/small | A |
| #714 | config: warn on unknown keys (docs spell `font_size`) | type/bug, priority/low, effort/small | A |

### Enhanced in this pass

- **Comments with current findings:** #134, #697, #167, #169, #261, #165, #166, #273,
  #268, #309, #336, #364, #209, #188, #365, #519, #621, #586, #652, #485, #492.
- **Retitled:** #309 (renderer performance design records), #364 (OSC 9;4 progress
  callback), #209 (provider-gated queries), #273 (agent palette clock), #166
  (Duplicate Tab position and palette command), #586 (Remove Host and Linux Help),
  #652 (retire at the next protocol bump), #288 (8,198 lines).
- **Relabelled:** priority and effort on the planned issues (#134, #167, #261 high;
  #169, #165, #336, #519 medium), and type plus priority on about 40 more.
- **Still unlabelled:** #492 has no labels, and #624 carries only
  `status/needs-triage`. Both wait on agent-integration.

### Loose ends from the pass

- Three comments name an issue "being filed alongside this one", written before the
  number existed. #167 and #169 mean **#707**. #165 means **#710**. A one-line
  follow-up comment on each would fix the links; it was not in the action list.
- The comments cite this note as "pending merge" on `docs/vision-refresh`. They stay
  right once the branch merges.

---

## Today: facts that change existing issues

| Fact | Evidence | Effect |
|---|---|---|
| One userdata context carries every libghostty callback | `CallbackCtx`, `crates/roost-vt/src/terminal.rs:321-348`; status callbacks `:352-359`, installed by `set_status_sink` at `:797-809`; `2c37afed` (2026-10-28). Desktop notifications use it since `7afc29ad` | #364, #209, #188, #365 unblocked. A new callback is "a field plus a trampoline" |
| Prompt-delta scrolling is at the pin | `GHOSTTY_SCROLL_VIEWPORT_DELTA_PROMPT`, `third_party/ghostty/src/include/ghostty/vt/terminal.h:332`, `delta_prompt` `:349` (pin `6ba14415`, `third_party/ghostty/build.sh:29`). roost-vt's `ScrollViewport` has `Top`, `Bottom`, `Delta` (`terminal.rs:90-100`) | #169 is S |
| A search API is at the pin, and its ABI moved after it | `ghostty_search_*`, `search.h:326-489` at the pin. Ghostty main `eb895f612` (279 commits later, 2026-11-08) adds `select_next`/`select_prev` and reshapes the C ABI (`657e82d1a`, `796f09a6f`, `0b7f9c156`). Nothing in `crates/` calls `ghostty_search_*` | #167 is M, and should follow a pin bump |
| The paste API with an unsafe-paste retry is at the pin | `paste.h:47-59`, `allow_unsafe` `:156`. Roost brackets pastes by hand (`crates/roost-ui-model/src/bracketed_paste.rs`) | #365's remaining value is a "Paste anyway?" prompt |
| Option slots exist for the rest | `OPT_ENQUIRY` `terminal.h:2225`, `OPT_SIZE` `:2250`, `OPT_COLOR_SCHEME` `:2260`, `OPT_PROGRESS_REPORT` `:2519`, `OPT_CLIPBOARD_READ` `:2636` | No upstream wait for #209, #364, #188 |
| The "what is in front" primitive exists | `ForegroundLeader` reads the child's `tpgid` with pid-reuse stamps; `find` returns `None` when the shell itself is in front (`crates/roost-engine/src/pty.rs:2377-2410`, `:2390`) | #519's "nothing uses the process-group leader" is stale. #519, #621 and #706 share it |
| Scrollback is a literal in two places | `crates/roost-iced/src/app/terminal_tab.rs:782` (`max_scrollback: 2_000`), `crates/roost-engine/src/tab_task.rs:62` (`SERVER_VT_SCROLLBACK = 2000`); lines, byte limit cleared (`crates/roost-vt/src/terminal.rs:43-45`, `:455-480`) | #707 |
| Sparkle checks are off on purpose | `mac/Resources/Info.plist.template:74-75` is `false`; the comment at `:27-30` says the explicit key suppresses Sparkle's first-run prompt and keeps checks user-invoked. `mac/scripts/update-appcast.py` emits no `releaseNotesLink` | #134 reverses a deliberate choice; the comment changes with it |
| The #336 lane passes | 7 of 7 `main` runs that reached the step since 2026-11-03 log `PASS: Iced real-seat Wayland`. Still `continue-on-error` (`.github/workflows/ci.yml:2376`), outside `ci-success`, and on `NON_BLOCKING` (`tools/roosttest_unit/test_ci_contract.py:35`) | Promote, then close #336 |

---

## Ranked top 15

Item numbers are stable. Other notes cite them (for example "item 13").

| Rank | Item | Kind | Impact | Effort | Issue | Plan |
|---|---|---|---|---|---|---|
| 1 | macOS tabs get no UTF-8 locale from a Finder/Dock launch | bug | High: every Finder or Dock launch, every tab | S | #697 | A |
| 2 | Sparkle background checks and release notes | distribution | High: decides whether later fixes reach Mac users | S | #134 | A (first) |
| 3 | Confirm before closing a running process or agent | safety | High for agent users | S–M | #706 | C |
| 4 | Find in scrollback | feature | High: agent transcripts are long | M | #167 | B |
| 5 | Configurable scrollback | feature | High: silent history loss | S–M | #707 | B |
| 6 | `roostctl` on PATH on macOS, for the skill | bug-ish | High for agent users on a Mac | S–M | #261 | C |
| 7 | Jump to previous / next prompt | feature | Medium-high | S | #169 | B |
| 8 | Clear a dead agent's state when the shell is back | agent | High; recommended in agent-integration key question 12 | M | #519, #621 | agent-integration Phase 3 |
| 9 | Terminal right-click menu | feature | Medium-high for new users | S–M | #165 | D |
| 10 | Clickable `path:line` | feature | Medium-high: agents print these all day | M | #708 | E |
| 11 | Reload config without a restart | feature | Medium: in-process restarts kill shells | M | #709 | E |
| 12 | Clear, select all, page scroll, move tab | feature | Medium: habits from every other terminal | S each | #710 | D |
| 13 | Resume agent conversations on restore | feature | Medium-high after reboots | M | unfiled | agent-integration Phase 7 |
| 14 | Ctrl/Cmd + wheel zoom | feature | Medium | S | #162 | D |
| 15 | Visual bell | feature | Medium-low | S | #163 | D |

Just outside: #365 (unsafe-paste prompt, M), #170 (hard restart, M, may need a
protocol decision), #339 (pill width, S once a metric is picked), #608 (toast cancels
a held drag, S–M), #188 (OSC 52 read with consent, M), #566 (warm tabs, M), #480 (the
session default for existing setups, a ruling).

---

## The plans

### Overview

| Plan | Goal | Issues | Complexity | Needs a ruling first | Depends on |
|---|---|---|---|---|---|
| A. v0.1.1 quick wins | Fixes reach Mac users; small debt cleared | #134, #697, #714, #712, #711, #713, #268, #336, #273; test for #570 | M (about ten S) | No | Nothing |
| B. Scrollback and find | History long enough for agents, searchable, navigable | pin bump, #707, #167, #169 | L | Scrollback default and unit | The bump, inside the plan |
| C. Agent safety | Roost never silently kills an agent; agents find `roostctl` | #706, #261 | M | Busy rule; Quit on sessions; #261 route | Coordinates with agent-integration Phase 3 |
| D. Everyday actions and the terminal menu | Habits from other terminals work | #710, #165, #162, #163 | M | Default keys | #710 before #165's Clear Scrollback |
| E. Paths and live config | Agent output is clickable; config edits apply live | #708, #709 | M–L | Editor template; host tabs | #714 (Plan A) helps #709 |

### Order

1. **Plan A** as v0.1.1. #134 goes in the first release out (see its open question).
2. **Plan C** next. It is small and protects the agent work that is Roost's reason
   to exist. Agent-integration's Phases 1–2 (env delivery, retire file delivery) can
   run beside it; they touch different code.
3. **Plan B.** The headline "real terminal" plan. The pin bump inside it is the
   riskiest step on this page.
4. **Plan D**, then **Plan E**, whenever there is room. D and E are independent;
   swap them freely.

Agent-integration's own order (its [Roadmap](agent-integration.md#roadmap), "Order")
is unchanged by this note.

---

### Plan A: v0.1.1 quick wins

**Goal.** Ship the fixes every Mac user needs, plus small cleanups, as one low-risk
release. Nothing here needs a ruling or a protocol bump.

**In scope, in order.**
1. **#134 Sparkle.** Set `SUEnableAutomaticChecks` true, add
   `SUScheduledCheckInterval` 86400, keep `SUAutomaticallyUpdate` false, and emit a
   `releaseNotesLink` per appcast item in `mac/scripts/update-appcast.py`. Rewrite the
   plist comment at `Info.plist.template:27-30`: an explicit key still suppresses the
   first-run prompt, it just says yes now.
2. **#697 locale.** In `tab_environment` (`crates/roost-engine/src/pty.rs:2592-2607`),
   when `LANG`, `LC_ALL` and `LC_CTYPE` are all unset on macOS, derive `LANG` per spawn
   (not `set_var`; tokio threads are running). Model: Ghostty (MIT)
   `src/os/locale.zig:17-27`, `:79-124`, fallback `en_US.UTF-8` at `:70-74`. The engine
   is shared, so session tabs get it too. Drop the `e2ec6678` test pin.
3. **#714 unknown config keys.** `warn!` for a key that is neither Roost's nor a known
   Ghostty key (`crates/roost-ui-model/src/config.rs:766-769`), with a hyphen and
   underscore "did you mean". Fix the spellings in
   `docs/getting-started/keybindings.md:61,63,120,122` and `docs/reference/themes.md:3`.
4. **Docs drift**, same commit as #714:
   - `keybindings.md:63,122` say font zoom is per-tab and not persisted. Since
     `8ded8291` it re-grids every tab and writes `font-size`
     (`crates/roost-iced/src/app/palettes.rs:748-765`).
   - `themes.md:10,80` say themes need a restart; Select Theme applies live.
   - `installation.md:7-12` names the apt repo without the commands, which are only
     in `README.md`.
5. **#712.** On Linux, check `DISPLAY` / `WAYLAND_DISPLAY` before the locks and
   `App::bootstrap` (`crates/roost-iced/src/main.rs:463-500`); print a hint and exit.
6. **#711.** Return early from `set_tab_title_from_osc` and `set_tab_cwd` when the
   value is unchanged (`crates/roost-engine/src/workspace.rs:1910-1968`).
7. **#713.** Log `NotFound` from `resize_with_px` at debug
   (`crates/roost-engine/src/session.rs:280-284`).
8. **#268.** Align `PaletteOpenParams` and `PaletteStateResult` doc comments
   (`crates/roost-ipc/src/messages.rs:615-617`, `:709-714`) and the
   `docs/reference/ipc.md:2984` row with `palettes.rs:980-999`. Close.
9. **#336.** Drop `continue-on-error` (`ci.yml:2376`), add `e2e-iced-wayland-drag`
   to `ci-success`'s `needs` (`:2727`), remove it from `NON_BLOCKING`. Close.
10. **#570 test.** "Resize away and back while Requesting" next to
    `a_withheld_resize_fires_at_finish` (`host_tab.rs:2465`).
11. **#273.** Tick an open agent palette on the existing 15 s clock; today the tick
    refreshes only the sidebar (`servicing.rs:3678`), and the palette rebuilds only on
    agent events (`servicing.rs:1453`).

**Stretch.** #166 (Duplicate Tab right of its source, plus a palette command), #661
(dead `forget_resume_on_regrid` calls), #586 (Remove Host with a confirm card).

**Out of scope.** Anything needing a ruling: #261's PATH route and #706's busy rule
(Plan C), the font-zoom baseline question (key question 7, for
[`chrome-themes.md`](chrome-themes.md)). #162 and #163 move to Plan D.

**Complexity.** M in total: about ten S items, no protocol change.

**Dependencies.**
- None inside the plan. Each item is its own commit.
- #697 reaches session tabs on an existing Mac only after the session binary is
  updated or restarted *(inference)*.

**Test approach.**
- #134: a bundle-script check that the production plist carries the three keys and
  the appcast item carries a notes link. Dev bundles are feedless and never check
  (`Info.plist.template:22-26`), so dev lanes are unaffected. Production-identity
  lanes need `-SUEnableAutomaticChecks NO`, which #134 already names.
- #697: a unit test of the derive rule (unset, set, empty, `LC_ALL` only) and a macOS
  lane check of `$LANG` through `tab.dump`.
- #714: parser unit tests for the warning list and the "did you mean".
- #712: a unit test of the display check.
- #711: a workspace unit test that an unchanged title or cwd bumps no revision and
  emits no event.
- #336: `tools/roosttest_unit/test_ci_contract.py` stays green.
- #273: an iced unit test that the clock message refreshes an open agent palette.
- Run the negative control on each new test.

**Open questions.**
1. Cut v0.1.1 as soon as #134 and #697 land, and the rest as v0.1.2? Every release
   before the one carrying #134 reaches only users who click Check for Updates.

---

### Plan B: Scrollback and find (with the Ghostty pin bump)

**Goal.** Keep enough history for agent transcripts, make it searchable, and let
users jump prompt to prompt. Search is an op, so `roostctl`, agents and the mobile
client get it too.

**In scope, in order.**
1. **Ghostty pin bump** from `6ba14415` to current main, as its own PR (the precedent
   is protocol-7 M1a, `discovery/protocol-7-release.md:185`). Adapt Roost to the new C
   ABI (enum numbering and setter names in `657e82d1a`, `796f09a6f`, `0b7f9c156`) with
   tests, rather than holding the old pin. No issue tracks the bump; it is a step of
   this plan.
2. **#707 scrollback key.** One key read by the in-process tab and the session VT
   (`terminal_tab.rs:782`, `tab_task.rs:62`). Measure with `tab.memory_usage` before
   picking the default (#366 is the memory side).
3. **#167 find.**
   - A `roost_vt::Search` beside each `Terminal`, fed and ticked on the main thread
     with a per-frame budget (`feed` and selection must be serialized with terminal
     access, `search.h:43-125` at the pin).
   - A `tab.find`-style op first, then the find bar and a highlight pass in
     `crates/roost-iced/src/terminal_widget.rs`.
   - Host tabs search the client mirror, which holds the hydrated scrollback and owns
     scroll and selection.
   - Keys: Cmd-F on macOS (unbound); Ctrl+Shift+F on Linux, since Alt-F would shadow
     readline's forward-word (`crates/roost-ui-model/src/keybind.rs:354-520`).
4. **#169 prompt jumps.** `ScrollViewport::DeltaPrompt(isize)` and two
   `KeybindAction`s. Marks come from Roost's bash and zsh integration; Apple's bash
   3.2 emits none.

**Out of scope.**
- Scrollback that survives a restart (L, unfiled; #363 closed with it open).
- Idle scrollback compression (#366).
- Searching on the session side for host tabs.

**Complexity.** L: the bump, then two or three feature commits.

**Dependencies.**
- The bump before find, so the search wrapper is written once.
- #707 before or with #167 and #169: the cap bounds both.
- Reading other notes: [`text-rendering.md`](text-rendering.md) also cares about the
  bump (its references sit at `eb895f612`). If a text-rendering plan bumps the pin
  first, this plan starts at step 2.
- Follow "adapt to upstream" on any break: fix Roost to the new contract.

**Test approach.**
- Bump: the existing byte-exact VT tests and the full gate, with the bump alone in
  its PR so any churn is isolated.
- #707: roosttest feeds more lines than the cap through `tab.feed_pty_bytes` and
  reads the retained length with `tab.dump` scrollback; a session-lane test that a
  host tab's mirror and the session VT agree.
- #167: roost-vt unit tests with fixture feeds (matches in scrollback and on both
  screens); roosttest through the new op; an iced unit test for highlight ranges.
- #169: roosttest with fed OSC 133 marks, asserting the viewport moves by prompts.

**Open questions.**
1. The scrollback default, and lines or bytes (Ghostty has both, `Config.zig:1394`,
   `:1412`; WezTerm defaults to 3,500 lines, `config.rs:1705-1707`).
2. For host tabs, does the session's cap win, or must client and session always
   match?
3. Does a hydrated mirror keep its semantic prompt rows? The `vt` payload is a text
   replay *(inference: it may not)*. If not, #169 works only on live output in host
   tabs until the session sends marks.
4. Does the bump change anything a protocol-7 peer sees? *(Inference: probably not,
   since the wire carries text replays, but check.)*

---

### Plan C: Agent safety

**Goal.** Roost never ends a running agent without asking, and an agent in a Mac tab
can run `roostctl` as the shipped skill expects.

**In scope.**
1. **#706 confirm close.**
   - A "busy" rule in the op set, so `tab.close` (and close project, and quit) can
     answer "would interrupt" to the UI and `roostctl` alike. The UI only renders a
     card.
   - Signals that exist: OSC 133 state, agent ownership, and `ForegroundLeader`
     (`pty.rs:2377-2410`).
   - Cover Cmd-W (`KeybindAction::CloseTab`, `crates/roost-iced/src/app.rs:6053-6066`),
     close project (today it asks only for 2+ tabs, `confirm_close_project`,
     `app.rs:7953`), Quit (`app.rs:5993-5996`) and the window close button
     (`window_settings`, `crates/roost-iced/src/main.rs:555-578`, keeps iced's
     default exit on close request).
   - References: Ghostty (MIT) `confirm-close-surface` on by default
     (`src/config/Config.zig:2628`); WezTerm (MIT)
     `skip_close_confirmation_for_processes_named` (`config/src/config.rs:809`).
2. **#261 `roostctl` on PATH on macOS.**
   - The bundle has `roostctl` in `Contents/Resources/bin/`
     (`mac/scripts/bundle-iced.sh:137-140`), but no tab gets that directory on
     `PATH`, and the skill calls bare `roostctl` (`skills/roost/SKILL.md:13`).
   - Route per the ruling: prepend `dirname(roostctl_path())`
     (`crates/roost-engine/src/process.rs:188`) to tab `PATH` on macOS, the issue's
     symlink action, or both.
   - Either way: a `cli.on_path` doctor check and one line in the skill.
   - Linux is fine: the package installs `/usr/bin/roostctl`
     (`packaging/nfpm.yaml:97`).

**Out of scope.**
- #519 and #621 (dead agent state): agent-integration
  [Phase 3](agent-integration.md#phase-3-process-layer-and-attribution-xl-as-two-plans-parallel-with-12).
- Resume on restore (item 13): agent-integration
  [Phase 7](agent-integration.md#phase-7-resume-on-restore-ml).
- Hook delivery and #485 / #492: agent-integration Phases 1, 2 and 4.
- Undo-close (hold the PTY a few seconds on a session backend). Later; note that
  Cmd-Shift-T is the command launcher on macOS (`keybind.rs:474`).

**Complexity.** M: #706 is S–M, #261 is S–M.

**Dependencies.**
- **Shared primitive with agent-integration Phase 3.** #706's busy rule and Phase 3's
  process stamps both read `ForegroundLeader`. Whichever plan lands first owns the
  shared "what is in front of this PTY" helper in `roost-engine`; the other reuses
  it. Neither waits for the other.
- #706 gets lighter once #480 moves existing setups to sessions, since Quit no longer
  ends shells there. Cmd-W still needs it.

**Test approach.**
- roosttest through `tab.close`: a tab running `sleep` in front answers "would
  interrupt" and stays open; with the force flag it closes; an idle shell closes with
  no prompt; an agent-owned tab (a fed OSC 7501 record) answers "would interrupt".
- iced unit tests that Cmd-W, close project, Quit and the close request all route to
  the one check and show the card.
- The window close button for real: the Mac real-input harness
  (`make e2e-iced-real-input-mac`), if a unit test is not enough.
- #261: a doctor unit test, and a macOS bundle-lane check that `command -v roostctl`
  resolves inside a tab (`tab.dump`).

**Open questions.** Key questions 2 and 3.

---

### Plan D: Everyday actions and the terminal menu

**Goal.** Habits from every other terminal work: clear, select all, scroll by page,
move a tab, right-click, wheel zoom, a visible bell.

**In scope.**
1. **#710.** `clear_screen`, `clear_scrollback`, `select_all`,
   `scroll_page_up/down`, `scroll_to_top/bottom`, `move_tab_left/right`, as
   `KeybindAction`s (`keybind.rs:133-180`) reachable from the palette. Move tab
   composes on `tab.reorder`. Today any modifier on PageUp/PageDown goes to the app
   (`bare_page_direction`, `crates/roost-iced/src/input.rs:98-118`).
2. **#165.** A `ContextTarget::Terminal` (today only `Tab`, `Project`, `Host`,
   `crates/roost-ui-model/src/context_menu.rs:17-24`) with Copy, Paste, Open Link and
   Clear Scrollback. The native macOS pop-up exists
   (`crates/roost-iced/src/macos/context_menu.rs`).
3. **#162.** Accumulate smooth-scroll deltas into whole font steps in `App::wheel`
   (`crates/roost-iced/src/app/interactions.rs:2264-2290`); mouse tracking keeps
   precedence. [`text-rendering.md`](text-rendering.md) lists it as its item 11;
   whichever plan comes first takes it.
4. **#163.** A flash on BEL and a `visual-bell` key. Bells already reach iced as a
   tab effect (`crates/roost-iced/src/app/servicing.rs:952,963`).

**Out of scope.** Prompt jumps (#169, Plan B). Find (#167, Plan B). Duplicate Tab
(#166, Plan A stretch).

**Complexity.** M: S each, M as a bundle with tests.

**Dependencies.** #710's `clear_scrollback` before #165's Clear Scrollback. Copy,
Paste and Open Link can ship first.

**Test approach.**
- roosttest: clear and clear-scrollback through `palette.activate`, checked with
  `tab.dump`; move tab checked with `tab.list` order.
- iced unit tests: the terminal context menu's items; wheel-delta accumulation; the
  bell flash state on a bell effect.
- Ghostty (MIT) defaults as the reference set: `Config.zig:6975-7044` (Linux),
  `:7154-7160` (macOS).

**Open questions.**
1. Default keys, especially on Linux, where Alt is the app modifier and shadows
   readline chords.
2. When mouse tracking is on, does right-click open the menu or go to the app?
   *(Inference: a Shift+right-click bypass is the common answer.)*
3. `visual-bell` default: on or off?

---

### Plan E: Paths and live config

**Goal.** A `path:line` an agent prints can be clicked open, and config edits apply
without a restart that would kill in-process shells.

**In scope.**
1. **#708 clickable paths.** Widen `roost-url` past its v1 scope
   (`crates/roost-url/src/lib.rs:6-14`, which names Ghostty's `src/config/url.zig` as
   the target) to paths with an optional `:line[:col]`; resolve against the tab's live
   cwd; link only if the file exists; open through an editor template. The open path
   exists (`crates/roost-iced/src/url_launcher.rs`).
2. **#709 reload config.** A `reload_config` action routed through the op set, so
   `roostctl` can trigger it. Re-parse (`Config::parse` is pure,
   `config.rs:550-772`), diff, apply what can change live, and report keys that still
   need a restart. A file watcher can come later. Ghostty (MIT) binds `reload_config`
   by default (`Config.zig:6676`).

**Out of scope.** A file watcher in the first cut. Opening paths that exist only on a
remote host, unless the ruling says otherwise.

**Complexity.** M–L: two M items. Split into two plans if review load is the concern.

**Dependencies.** #714 (Plan A) first, so a reload can report unknown keys. #708's
editor key is a config key, so #709 picks up changes to it.

**Test approach.**
- #708: extend the corpus in `tests/url-fixtures/`; unit tests for the cwd resolve and
  the existence check.
- #709: roosttest with an isolated `ROOST_CONFIG`, never the developer's real config:
  edit the file, reload through IPC, assert the new keybind or palette entry; a reload
  with an unknown key reports it.

**Open questions.**
1. The editor template: a config key, with `$EDITOR` or `code -g` fallbacks?
2. Host tabs: link only local tabs at first?
3. Which key groups apply live, and which say "restart needed"?

---

## Coordination with agent-integration

[`agent-integration.md`](agent-integration.md) owns these. This note does not plan
them:

| Issue or item | Agent-integration phase |
|---|---|
| #519, #621 (dead agent state; ForegroundLeader) | Phase 3 |
| #623, #624 (attribution) | Phase 3, when enforcement flips |
| #485, #492 (file delivery side effects; holding comments posted) | Phases 2 and 4 |
| #622 (Claude hook contract) | Phase 1, if it rides along |
| #553 item 2 (quit abandons agent-hooks work) | Shrinks in Phase 2 |
| #649 (7501 child rows) | Phase 5 |
| #516 (tab prompt after a stream loss) | Phase 6 |
| Resume on restore (item 13, unfiled) | Phase 7 |
| #364 (OSC 9;4 progress callback) | Phase 8 |
| #508, #560, #509 | "Track, not plan" |

The one shared seam is `ForegroundLeader`, used by Plan C's #706 and Phase 3. See
Plan C's dependencies.

---

## Not in a plan yet

- **Parked until a consumer appears:** #476, #498, #504, #505, #507, #508, and #506
  (an event-driven `wait --text`). Whether to move them to a docs list is key
  question 10.
- **Unblocked but low priority:** #209 (provider-gated queries), #188 (OSC 52 read
  with consent), #365 (unsafe-paste prompt).
- **Rulings first:** #480 (session default for existing setups), #339 (pill metric),
  #170 (hard restart; may need a protocol decision).
- **Deliberately parked:** #297 (multi-click timing; porting would regress idle
  double-clicks, per its 2026-08-05 comment).
- **Next protocol bump:** #652.
- **Design records:** #309 (renderer performance), #288 (interactions split).
- **Remaining bugs:** #600, #300, #630, #597, #631, #553, #654, #608, #518, #683, #568.
- **Test and CI:** #390, #395, #660, #633, #322, #523, #275, #385, #482, #495.
- **Terminal and Mac:** #186, #187, #676, #366, #582, #117, #599, #662, #586.
- **Chores:** #661, #367, #302, #168. `FEATURES.md` still mentions GTK, and
  `packaging/roost-gtk-alias.desktop:10` says "Revisit at v0.1.0".

---

## Unfiled candidates

Rows marked *ruling* go to Charlie as questions, not issues.

| Candidate | Impact | Effort | Pointer |
|---|---|---|---|
| `roost` ignores argv: no `--version`; `Exec=roost %F` drops its paths | Low-medium | S | no `env::args` in `crates/roost-iced/src`; `packaging/roost.desktop:11` |
| A renamed tab's title lock can't be cleared | Low-medium | S | `rename.rs:9-12`; only `workspace.rs:1897` writes `user_titled` |
| Command-finished notification for a background tab | Medium | S–M | OSC 133 C/D parsed (`program_status.rs`); Ghostty `notify-on-command-finish`, `Config.zig:1214`. Overlaps agent-integration Phase 3's "exited (code)" banner |
| Notification mute / DND, sound, window attention | Medium-low | S–M | no `request_user_attention` in `crates/roost-iced/src`; `macos/notifications.rs:160` |
| Default shell and working-directory keys | Low-medium | S | `resolve_argv`, `pty.rs:2141-2151` |
| Surface a crash report on next launch | Low | S | `crates/roost-engine/src/crash.rs` |
| Move a tab to another project | Low-medium | S–M | `TabReorderParams` is per project |
| Tab / project colors | Low | S–M | no color field on `RestoreTab` / `RestoreProject` |
| Scrollback persistence across restarts | Medium | L | #363 closed with this half open; herdr `pane_history` is the opt-in model |
| Terminal user-theme directory and inline color keys | Low-medium | S | `docs/reference/themes.md:78`. Light themes belong to [`chrome-themes.md`](chrome-themes.md) |
| Homebrew cask, RPM | Low-medium | S / M | *ruling*; #83 closed them as non-goals |

Not worth filing: a Linux update notice (the apt candidate on this box is 0.1.0, so
the channel works), and a quick terminal with a global hotkey (M–L, against "one
window").

---

## Log-derived bugs

Logs read: `~/.local/state/roost/roost.log` (production UI, 0.0.23),
`~/.local/state/roost-session/roost.log`, `~/.local/state/roost-iced/roost.log`.

1. **No-op title and cwd commits** → #711 (Plan A). Each commit copies `state.json` to
   `.bak` and rewrites it (`crates/roost-engine/src/persistence.rs:276`) and
   broadcasts. The session log shows a persist attempt every 0.96 s while the disk was
   full (`roost-session/roost.log:148-171`, 2026-10-23); the source is unverified
   *(inference: an animated agent title)*. Low impact: `state.json` is about 2 KB.
2. **Display-less launch panics mid-bootstrap** → #712 (Plan A). Three crash files
   (2026-10-22) carry the winit "Create event loop" error.
3. **"localhost session cannot start; not retrying", repeated**
   (`roost/roost.log:910-926`, 0.0.23 era). Reworded on main
   (`crates/roost-session/src/start.rs:223-241`). Re-test on main before filing.
4. **Log noise.** `pty resize failed err=NotFound` → #713 (Plan A). `sctk_adwaita`
   unknown-button warnings on COSMIC are upstream. Test-mode runs writing into the
   developer's dev log belong on #523.

The repo has no `XXX`, `FIXME` or `HACK` markers to mine.

---

## Covered elsewhere or decided

- **Accessibility / VoiceOver** was accepted as a trade in DL-16
  (`docs/development/decision-history.md`).
- **No session autostart:** #454 closed by decision.
- **Durable notification history** is out of scope (`docs/guides/notifications.md`).
- **Linux Alt as the app modifier** shadows readline chords. A "readline-friendly"
  keybind recipe in `keybindings.md` covers it without changing defaults.
- **Light themes and following the system theme** belong to
  [`chrome-themes.md`](chrome-themes.md) (#164).

---

## Key questions

1. Plan A: cut v0.1.1 as soon as #134 and #697 land, and the rest as v0.1.2?
2. Plan C, #261: a tab `PATH` prepend on macOS, the symlink action, or both?
3. Plan C, #706: what counts as busy (any foreground job, agent-owned only, or not at
   a prompt)? Should Quit ask on the session backend, where shells survive?
4. Plan B: scrollback default, in lines or bytes? Must session and mirror caps always
   match?
5. Agent-integration Phase 3: end ownership once the agent's process is gone (#621,
   #519)? Asked there as key question 12, in user terms, with a recommendation of
   yes ([Ownership after an agent dies](agent-integration.md#ownership-after-an-agent-dies)).
   Listed for completeness.
6. Resume on restore: on by default, offered, or not at all? (Agent-integration
   Phase 7.)
7. Should a font zoom persist as the next launch's baseline, as it does since
   `8ded8291`, or stay per session? Plan A fixes the docs to today's behavior either
   way.
8. #339: which pill metric?
9. #480: is the mobile client reason enough to schedule the default flip?
10. Move the "when a consumer appears" issues to a docs list and close them?
11. Plan E, #708: the editor template, and local tabs only at first?

---

## Corrections

To the first draft of this note:
- **#273:** the draft inferred the agent palette re-renders on the 15 s tick. It does
  not: the tick calls only `refresh_sidebar_agents` (`servicing.rs:3678`). #273 is
  retitled to that gap and is in Plan A.
- **#309:** the draft said to keep item 6 (release-profile CI). It is done: the
  `iced-release` job (`ci.yml:2007`, E2E step `:2113`) is in `ci-success`. #309 now
  holds only design records.
- **#336:** the draft's evidence was a sample. The pass checked step logs by hand:
  7 of 7 `main` runs that reached the step passed.
- **#570** is closed without its test; Plan A adds it.

To the research reports:
- Report 1 dated the callback multiplexing to `7afc29ad`, which added the
  desktop-notification callback. The shared context is `2c37afed`.
- Both reports called #167's search API a new finding. #167's 2026-10-28 comments
  already recorded it. The new finding is that the ABI moved after the pin.
- Report 2's Linux update-notice candidate is dropped: the apt candidate is 0.1.0.
- Report 2 rated the `state.json` rewrite a latent bug. It is deliberate
  write-through (`workspace.rs:1921-1922`); only the missing no-op check is a defect.
- Report 2 said zellij was not cloned. It is at `thirdparty/zellij` (`43c8710`, MIT).
- Report 1's skill pointer is `SKILL.md:13`, not `:16`.

---

## References

| Reference | License | Use |
|---|---|---|
| `third_party/ghostty/src` at `6ba14415` (the pin) | MIT | `terminal.h`, `search.h`, `paste.h` |
| `thirdparty/ghostty` (`eb895f612`) | MIT | `src/os/locale.zig`, `src/config/Config.zig`, `src/config/url.zig`; commits after the pin |
| `thirdparty/herdr` (`ee5bfb5`) | Apache-2.0 | `src/pane.rs:384-411`, `src/detect/mod.rs:377`, `src/agent_resume.rs:58`, `src/persist/restore.rs` |
| `thirdparty/wezterm` (`458a100`) | MIT | `config/src/config.rs:809`, `:1705-1707`, `:1727` |
| `thirdparty/zellij` (`43c8710`) | MIT | License checked; not needed |
| `thirdparty/cmux` (`a79cbde73ba`) | GPL-3.0-or-later; some server directories BUSL-1.1 | Only the LICENSE was read |
| `thirdparty/cosmic-term` (`9129277`) | GPL-3.0 | Only the LICENSE was read |
| iTerm2 (not cloned) | GPL-2.0 | Feature-level knowledge only |

Roost sources: [`agent-integration.md`](agent-integration.md),
[`chrome-themes.md`](chrome-themes.md), [`text-rendering.md`](text-rendering.md),
[`split-panes.md`](split-panes.md), [`protocol-7-release.md`](protocol-7-release.md),
[`swift-retirement.md`](swift-retirement.md), [`agent-watching.md`](agent-watching.md),
`CHANGELOG.md` (v0.1.0), and the issues cited above.
