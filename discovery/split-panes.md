# Split panes that agents can drive (discovery)

Status: **discovery**, a note to understand the space for a future planning
session. It isn't a commitment to build now. Written 2026-11-08 and
rewritten the same day around Charlie's goals. File and line references are
as of `main` @ `68231b9b`. The "no split-pane" non-goal was lifted the same
day (DL-35 in `docs/development/decision-history.md`), but splits are **not
planned**: they are a possible future feature, and the other discovery items
(agent integration, the backlog plans, chrome comfort, text) come first.
Until a plan lands, a tab is one terminal.

**Timing:** not scheduled. After v0.1.0 (the Swift app is retired, DL-33),
so the work is iced only and covers macOS and Linux. No issue tracks splits;
a search of open and closed issues found none.

---

## Summary

- **Charlie's goals (2026-11-08).** If splits are built, they should:
  1. let **agents control the panes**, the way herdr and cmux stress;
  2. run **several agents in parallel in one window**;
  3. put **a plain terminal beside an agent**: a git diff viewer or a file
     browser in one split, the agent in another.

  It should feel like Ghostty, cmux or herdr, with Roost's native chrome.
- **A client-only split view does not meet these goals.** A 2-up (C2) or a
  window-owned tree (B) keeps the layout in the window. An agent on a
  remote host (#560, open) or on a session with the window closed cannot
  split anything. Other clients and the phone never see the layout. C2 also
  caps parallel agents at two.
- **Every agent-driven multiplexer exposes the same small API.** It has
  seven parts:
  - a per-terminal id in the environment;
  - a split that returns the new id and does not move focus;
  - send text or keys;
  - read the screen or scrollback;
  - wait for output or state;
  - list the layout;
  - close.

  herdr, cmux and tmux have all seven. WezTerm lacks a wait. Ghostty has
  split, input and close only through macOS AppleScript.
- **Roost already has most of it, at the tab level.** `ROOST_TAB_ID`,
  `tab.open` with `activate: false` (#503), `tab.write`, `tab.dump`,
  `roostctl wait`, `roostctl tab prompt`, `events.subscribe` and the agent
  axes. Missing: a placement ("open beside this terminal"), a layout read,
  ratio/zoom/swap, and two safety rules (see "Agent control gaps").
- **Recommendation: a session-owned layout of tabs ("H", the concrete form
  of option G).** The user sees panes inside a tab, like Ghostty and herdr.
  On the wire a `tab` stays one terminal, and a new workspace record groups
  tabs into a split tree. It lives in `roost-engine`, so the in-process
  backend and `roost-session` serve it from one implementation.
- **Why not literal panes-inside-a-tab (A).** Every reference keeps a
  stable *per-terminal* id in the environment (`TMUX_PANE`,
  `WEZTERM_PANE`, `ZELLIJ_PANE_ID`, `HERDR_PANE_ID`). `ROOST_TAB_ID` already
  is that id. A would rename the terminal, mint a new id, migrate hooks and
  bump the session protocol, and buy nothing H does not.
- **What H keeps:** `ROOST_TAB_ID`, hooks, every `tab.*` op, the four
  per-tab agent axes, size ownership per terminal (DL-31), the Agents panel.
  **What it adds:** a layout record, `split` on `tab.open`, about eight
  `layout.*` ops, one replayed event, `layouts` on `Project` and in
  `state.json`. All additive: **no session protocol bump**.
- **One rule splits state: structure is shared, the view is per client.**
  The tree and its ratios are workspace state. The focused pane (simply the
  active tab) and zoom belong to each window.
- **Phases.** Groundwork for agent control (S-M), engine and in-process
  splits (L), sessions and hosts (L), agent-driven layouts (M), an optional
  tmux shim for Claude Code teammates (spike S, then M), mobile (M).

---

## Goals and what they require

| Goal | What the design must provide |
|---|---|
| 1. Agents control panes | Layout ops in the **op set**, served wherever an agent's tab dials: the UI socket and a session socket with no window. Ids an agent can hold. A default target of "the caller", never "the focused pane". No focus theft. A layout read. Safe close. |
| 2. Parallel agents in one window | N panes, each with its own agent state, badge and notification. A pill rollup. The Agents panel still lists one row per agent. Size ownership that copes with N terminals shown at once. |
| 3. A terminal beside an agent | A split that opens in the agent's cwd (`cwd_from_tab`), runs any argv (`git diff`, a file browser), keeps its place across relaunch and shows on every client. A declarative way to set it up once ("agent left, diff right"). |
| Native chrome | Ghostty/cmux/herdr gestures: split, close pane, directional focus, zoom, divider drag. A pane header with the agent's state. Iced, not box-drawing borders. |
| Keep the agent features | A badge, a hook and `ROOST_TAB_ID` each still name one terminal. |

---

## Today

### Where the non-goal came from

As of `68231b9b`:

| Where | Text at `main` |
|---|---|
| `CLAUDE.md:56`, `:327` | "Sidebar of projects, tabs per project, one terminal per tab." / "Not split-pane. One terminal per tab, period." |
| `docs/development/vision.md:9`, `:178` | "…one terminal per tab." / "**No split-pane.** One terminal per tab." |
| `FEATURES.md:61-62` | "**Split panes** — one terminal per tab. Notification routing gets weaker the more places output can land." The only stated reason. |
| `docs/index.md:32` | "Not split-pane — one terminal per tab." |
| `docs/guides/automation.md:14` | "…has one window, no splits, no panes." |
| `skills/roost/SKILL.md:57` | "A tab is one terminal running one shell or command. Roost has one window and no splits or panes." Agents read this, so it is a contract too. |
| `discovery/mobile-client.md:89`, `discovery/sessions-everywhere.md:37` | Restate it as a project-wide non-goal. |

- **History (verified with `git log -S`).** `8436bf83` (2026-04-26, the Go +
  GTK4 scaffold) added the `CLAUDE.md` line next to Windows, Electron, an
  embedded browser and multi-window. `4cb33d3b` (2026-04-29) added the
  `FEATURES.md` reason. No DL entry before DL-35 covered it.
- **Now.** The 2026-11-08 vision refresh (DL-35) rewrote the first four rows
  and the two discovery notes. The guide and the skill still describe
  today's product and change only when a split ships.
- **Earlier discovery pointed here.** `discovery/host-sessions.md:498-499`:
  "Splits, if we want them, as a Roost layout on the session, not a nested
  muxer." `discovery/sessions-everywhere.md:38-42` adds the hard limit: a
  PTY has one size, so a full-screen TUI cannot be laid out differently for
  two screens at once.
- **The `FEATURES.md` worry, per shape (inference).** If a tab became
  several terminals (A), the tab badge would become an aggregate, which is
  the weakening it warns about. Under H a badge still names one terminal,
  and the pill's aggregate is the same kind of rollup the project row
  already is.

### One active tab in the core

- `Workspace::Inner` holds flat `tabs: BTreeMap<i64, TabRow>`, one
  `active_project_id`, one `active_tab_id`, and `active_tabs_by_project`
  (`crates/roost-engine/src/workspace.rs:170-183`).
- `select_tab` sets the single pair (`workspace.rs:2901`).
- Ids come from one counter, `alloc_id` (`workspace.rs:2938`), which only
  grows.
- Every mutation lands through `commit` (`workspace.rs:2742`), which bumps
  one revision per batch.
- A tab is listed only once its PTY exists (#640, closed):
  `open_and_spawn` reserves the row, spawns, then publishes
  (`crates/roost-engine/src/application.rs:196-197`;
  `reserve_tab` `workspace.rs:1692`, `publish_tab` `:1735`).
- `TabsReordered` already exists as an event, fired after `reorder_tabs`
  (`workspace.rs:2438`).

### Identity: `ROOST_TAB_ID` is already a pane id

- Every PTY gets `ROOST_TAB_ID`, `ROOST_SOCKET` and `ROOST_AGENT_HOOK`
  (`crates/roost-engine/src/pty.rs:2719-2733`).
- A `roostctl` verb that takes `--tab` falls back to `ROOST_TAB_ID`
  (`skills/roost/SKILL.md:24`). That is the "default to the caller" rule
  herdr, WezTerm and tmux follow.
- The wire `Tab` carries every agent axis: `state`, `has_notification`,
  `shell_state`, `agent_lifecycle`, `ownership`, `program_status`,
  `password_input` (`crates/roost-ipc/src/messages.rs:61-120`).
- **A new tab does not inherit its opener's environment.** It gets the
  serving process's environment plus at most four opener names
  (`DISPLAY`, `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`,
  `SSH_AUTH_SOCK`; `crates/roost-ipc/src/opener.rs:23-28`, DL-32). So an
  agent that opens a terminal beside itself does not leak its `CLAUDECODE`
  or session id into it. The residual risk is the server's own environment:
  `pty.rs` scrubs no `CLAUDE*` and no `TMUX` from it (grep; no match).

### "Being watched" is one tab

- In-process: `tab_is_being_watched` is `window_focused && active_tab_id
  == tab_id` (`workspace.rs:2934-2936`).
- Host and session tabs: the iced twin `host_focus_claim`
  (`crates/roost-iced/src/app.rs:2051-2053`) returns one `Option<TabKey>`.
  Suppression for host tabs is client-local (#474, closed).
- Focusing a tab acknowledges its notification (`docs/reference/ipc.md:1457-1461`).

### The iced window draws exactly one terminal

- `view_body` (`app.rs:6870`) builds one `TerminalWidget`, for
  `active_tab_key()`, at `app.rs:7259`. The notice overlay wraps it at
  `:7297`. The "Sized by X" badge is in the tab band for the active key
  (`:7251`). Then `column![tab_bar, terminal]` (`:7301`).
- **The widget is not a singleton** (`crates/roost-iced/src/terminal_widget.rs:758-794`).
  It carries its own tab, snapshot, metrics, IME and focus flags, and maps
  pixels to cells from its own bounds.
- **IME is safe with several widgets.** A widget asks for
  `InputMethod::Disabled` unless `ime_active`, iced 0.14's merge keeps the
  first `Enabled`, and `terminal_ime_active` is already keyed by tab
  (`app.rs:2531`).
- **The cursor-focus flag is not keyed.** `terminal_cursor_focused(route,
  window_focused)` (`app.rs:2540`) has no tab argument, so two widgets
  would both draw a solid cursor.
- **iced diffs children by position** (`iced_core-0.14.0/src/widget/tree.rs:71-100`:
  truncate, then zip). A structural edit would hand one pane's
  `TerminalWidgetState` to another. `agents_split.rs:1-9` shows the house
  fix: both children always present, a hidden one at zero size.
  `iced_widget-0.14.2/src/keyed.rs` is a model for keyed children.

### The grid comes from the window

- `App::current_grid` (`app.rs:5592`) is the grid every new tab spawns at
  and every host attach dials with.
- `regrid_window` (`app.rs:1901`) applies one `(cols, rows)` to every tab,
  shown or not.
- The background wave sends every non-attached local-session tab the same
  grid (`crates/roost-iced/src/app/background_resize.rs:54-68`).
- **This is the main iced change any split needs:** a grid per pane,
  computed from a model-side tree.

### Input, pointer and attach go to the active tab

- `resolve_keyboard_route` ends in `KeyboardRoute::Terminal(active_tab)`.
- `App::pointer` passes `on_screen = Some(self.active_tab_key())`
  (`crates/roost-iced/src/app/interactions.rs:2102`). The reconcile at
  `crates/roost-iced/src/app/servicing.rs:1472-1476` says "The pointer can
  only be over the tab on screen".
- `active_tab_key()` has about 30 call sites in 6 files (grep count; not
  every one breaks).
- `host_attach` "keeps exactly one entry alive at a time today … keyed like
  `tabs` so C6/C7 can widen that policy" (`app.rs:3569-3574`). DL-18 is
  attach-on-focus. #566 (open, warm tabs) widens it.
- The session admits many data connections per tab
  (`crates/roost-engine/src/ipc.rs:1268-1280`), capped at
  `MAX_DATA_CONNS_PER_SESSION = 32`, whose doc assumes "a tab is shown by
  one client at a time" (`ipc.rs:1195-1199`).

### Size ownership is per tab (DL-31, protocol 7)

- A session tab has at most one size owner, an attachment. Non-owners draw
  the owner's grid inside their view with a "Sized by X" badge
  (`crates/roost-engine/src/size_owner.rs`; rules in
  `discovery/protocol-7-release.md:950-1010`).
- Takes: an explicit tab switch, keys and paste, clicks and the wheel, a
  copy, a settled scroll, a resize while the window has focus, Take size.
  Not a take: showing a tab, an automatic re-attach, an unfocused resize,
  `tab.write`.
- Font size is per tab (`FontIncrease`, "Per-tab font adjusters",
  `crates/roost-ui-model/src/keybind.rs:84-96`), so panes can have
  different cell sizes.

### Persistence

- `SnapshotFile` restores the active tab **by position**, because tab ids
  are not stable across restore (`crates/roost-engine/src/persistence.rs:48-52`).
- `ProjectSnapshot` (`persistence.rs:208-224`) holds a flat
  `Vec<TabSnapshot>` and `last_tab_position`. No tree anywhere.
- `agents_panel_fraction` (`:68-75`) is the precedent for an optional field
  omitted while `None`.
- `HostTabMemory` / `TabMemo` (`:188-206`) is client-side memory of a
  host's tabs.
- "Layout" already means persisted tab descriptors in code:
  `RestoreLayout` (`workspace.rs:324`), `take_restore_layout` (`:1060`),
  DL-7 "Tabs persist as layout". A new entity needs a name that does not
  collide.

### Reusable pieces

- `crates/roost-iced/src/agents_split.rs`: one widget, whole-pixel layout
  from one fraction, both children always present, an epoch. Copy it, not
  `sidebar_resize.rs` (#630, open: no epoch).
- `crates/roost-iced/src/resize_grip.rs`: the shared drag grip.
- `ChromePalette.split_handle` / `split_handle_hover`
  (`crates/roost-iced/src/chrome.rs:275-276`).
- `project_rollup` (`crates/roost-ui-model/src/rollup.rs:48-52`): the
  highest-ranked lifecycle of a set of tabs. A pill rollup is the same
  function over a layout's leaves.
- iced 0.14 ships `pane_grid` with its own mutable `State`. A model-owned
  tree feeding a custom widget fits the north star better (inference).

---

## Agent control today and its gaps

What an agent inside a Roost tab can already do, and what herdr or cmux
would add.

| Need | Roost today | Gap |
|---|---|---|
| Know its own terminal | `ROOST_TAB_ID`, `ROOST_SOCKET` | None |
| Open a terminal without stealing focus | `roostctl tab open --no-activate` / `open --no-activate` (#503); the skill says never `--focus` unless asked (`SKILL.md:146`) | None for a tab; no "beside me" placement |
| Place it next to itself | `tab open --after-tab ID` | Done client-side as an open followed by a separate `tab.reorder` (`crates/roost-cli/src/main.rs:1538-1557`). Not atomic. A split needs placement in the same commit. |
| Send input | `tab.write`, `roostctl tab send` | None |
| Prompt another agent and wait for its turn | `roostctl tab prompt` | **Does not refuse a blocked target.** It writes the text, then Enter, whatever the target's state; only an already-running turn changes the flow (`crates/roost-cli/src/prompt.rs:160-175`). Into a permission dialog, that Enter could approve something. herdr refuses with `agent_blocked`. |
| Read output | `tab.dump` (viewport and scrollback) | None for this goal |
| Wait | `roostctl wait --state / --text / --gone` (`crates/roost-cli/src/wait.rs:25-51`); `--text` polls `tab.dump` | Client-side polling, not server-owned. Enough to start. |
| Read the layout | none | `layout.get` |
| Resize, zoom, swap, move | none | `layout.*` ops |
| Close safely | `tab.close`, unguarded | No record of who opened a tab. herdr relies on skill rules only. |
| Reach the window from a session tab | `roostctl` routes `tab open`, `tab focus` and more to the owning window (`docs/reference/cli.md:65-95`) | Remote-host tabs cannot reach a window (#560) |

---

## How reference terminals let agents drive panes

Licenses were checked before reading. cmux is GPL-3.0-or-later: only its
README and docs prose were read, at design level; no code was read, quoted
or paraphrased. Claude Code is proprietary; its teammate backend was
observed through `strings` on the installed 2.1.319 binary, for behavior
only.

| | herdr | cmux (design) | Ghostty | Zellij | WezTerm | tmux |
|---|---|---|---|---|---|---|
| License | Apache-2.0 | GPL-3.0+ | MIT | MIT | MIT | ISC |
| Transport | JSON socket + CLI | socket + CLI | macOS AppleScript and App Intents; Linux D-Bus only for new window or tab | CLI to server; plugins | `wezterm cli` to mux | CLI; control mode |
| Self env | `HERDR_PANE_ID`, `_TAB_ID`, `_WORKSPACE_ID`, `HERDR_ENV`, `HERDR_SOCKET_PATH` | `CMUX_SURFACE_ID`, `_WORKSPACE_ID`, `_TAB_ID` | `GHOSTTY_SURFACE_ID` | `ZELLIJ_PANE_ID` | `WEZTERM_PANE` | `TMUX`, `TMUX_PANE` |
| Default target | caller, then focused | caller (env defaults) | caller (`+new-tab` only) | **focused** for most verbs | caller, then last-active client's focus | client tty, then `TMUX_PANE` |
| Split takes focus | no (`focus` defaults false) | never from the socket | yes | yes; `--no-focus` opt-in | no flag | yes; `-d` opt-out |
| Read | 4 sources, alt-screen history | `read-screen`, `capture-pane` | visible, full, selection (Intents) | `dump-screen`, `subscribe` | `get-text` | `capture-pane` |
| Wait | output match; agent state, server-owned | yes | no | `--blocking`, `--watch` | no | `wait-for` |
| Agent status | detection + `pane.report_agent` | hooks + OSC 7501 | bell, progress | OSC 7501 + 8 other sources | user vars | activity, silence |
| Rollup | pane to workspace by priority | pane ring, surface badge, workspace row | none | pane, tab, session, global | none | window flags |
| Close safety | none in the API beyond a worktree-group confirm; skill rules | focus policy; socket trust modes | `withConfirmation: false` | none; plugin permissions | "immediately and without prompting" | none |
| Pane chrome | accent border and bold title | ring, per-pane tab bar with badges | 0.7 unfocused opacity, divider color | frames with title, attention marker, other clients' focus | inactive HSB 0.8/0.9 | border-status header, active border, inactive style |

### herdr (Apache-2.0, read in depth): the best permissive template

- **Model.** Workspace → tab (a binary split tree) → pane (one terminal).
  An *agent* is a recognized process in a pane, with an optional unique
  name.
- **Ids.** `w1`, `w1:t1`, `w1:p1`: opaque, never reused. A move across
  workspaces mints a new pane id; the old one resolves only for the moved
  process's inherited env (`skills/herdr/SKILL.md:63-69`).
- **Env.** `HERDR_ENV=1` plus workspace, tab and pane ids. Before a pane
  launches it **removes** `CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`,
  `CODEX_THREAD_ID` and similar, "A new pane is not a child agent of the
  process that started the server" (`src/pane.rs:184-220`).
- **Split.** `PaneSplitParams` takes direction, `ratio`, `cwd`, `env` and
  `focus`, which defaults to **false** (`src/api/schema/panes.rs:21-41`).
  Focus moves only when asked (`src/app/api/panes.rs:120-125`). The new
  pane's size is computed before the spawn (`panes_after_split`,
  `src/layout.rs`). Ratios clamp to 0.1-0.9 (`valid_split_ratio`,
  `src/layout.rs:628-634`).
- **Target.** The CLI fills the target from `HERDR_PANE_ID`; the handler
  falls back to workspace focus, then active focus
  (`src/app/api/panes.rs:34-48`). The skill: "Other commands may use the
  UI-focused pane, which can belong to the user or another client"
  (`SKILL.md:77`).
- **Layout as data.** `layout.export` / `layout.apply` take a portable tree
  of `{split, direction, ratio, first, second}` and `{pane, cwd, command,
  env}`. `layout.apply` rebuilds structure and commands, not live PTYs.
- **Agent ops.** `agent.prompt --wait` refuses a blocked agent with
  `agent_blocked` and writes nothing (`src/app/api/agents.rs:171`, test
  `:767`). `agent.wait` is server-owned.
- **Rollup.** Blocked 4, done-unseen 3, working 2, idle-seen 1, unknown 0,
  maximum over every pane (`src/workspace/aggregate.rs:50-75`).
  Suppression is by **visible tab**, not focused pane
  (`src/app/actions.rs:38-43`, `:375-385`). "Seen" is per client.
- **Safety lives in the skill** (`SKILL.md:204-214`): use `--no-focus`;
  target with `--current`, an id or an agent name, never "the focused
  pane"; parse ids from JSON; do not close what you did not create; never
  stop the server. The API's close confirms only when a worktree group
  would close (`src/app/api/panes.rs:1967-1988`).
- **Parallel agents.** A sidebar agents section across workspaces (the same
  idea as Roost's plan-080 Agents panel), named agents, and
  `worktree.create` for one checkout per agent.

### cmux (GPL-3.0-or-later; design level, docs prose only)

- **Model.** Workspaces as vertical tabs; a split tree per workspace; each
  pane has its own tab bar of surfaces. Its split engine is bonsplit.
- **Surface.** A tmux-shaped CLI over a socket (`docs/cli-command-tree.txt`):
  new-split, list-panes, focus-pane, capture-pane, read-screen, send,
  send-key, resize-pane, break-pane, join-pane, plus notify, set-status and
  an events stream.
- **Focus policy.** Socket commands never raise the app. Only an explicit
  list of focus verbs may change in-app selection; commands that open
  things focus only when run interactively
  (`docs/socket-focus-steal-audit.todo.md`, `skills/cmux-socket-policy/SKILL.md`).
- **Agent teams.** `cmux claude-teams` "runs Claude Code's teammate mode
  with one command. Teammates spawn as native splits with sidebar metadata
  and notifications. No tmux required" (`README.md:83`). The CLI contract
  lists an internal `__tmux-compat` dispatcher (`docs/cli-contract.md:87`,
  `:248`). Design reading: a launcher sets the environment and puts a tmux
  stand-in first on `PATH` for that one process.
- **Trust boundary.** Socket modes include "only processes started inside
  cmux", a password, or allow-all (`docs/configuration.md`).

### Claude Code's teammate backend (proprietary; strings only)

From `strings` on the installed 2.1.319 binary:

- `teammateMode` is described as "How spawned teammates execute (tmux,
  iterm2, in-process, auto)". The backend is chosen as tmux when
  `process.env.TMUX` is set ("Selected: tmux (running inside tmux
  session)").
- The tmux verbs it uses include `display-message -p '#{pane_id}'`,
  `split-window -d -t <pane> -h -l 70% -P -F '#{pane_id}' -- <cmd>`,
  `list-panes`, `select-layout`, `select-pane -T`, per-pane
  `pane-border-status` / border styles, and `kill-pane`.
- **Implication (inference, nothing run).** A narrow shim that answers
  these verbs would turn Claude Code teammates into native Roost panes.
  That is cmux's design. Claude Code also has an `in-process` mode, so a
  shim is an enhancement, not a requirement.

### Ghostty (MIT)

- Window → tab → immutable split tree → surface
  (`src/datastruct/split_tree.zig`). Every surface gets
  `GHOSTTY_SURFACE_ID` (`src/Surface.zig:680`).
- **No split, read or send IPC on Linux.** On macOS, AppleScript has
  `split <terminal> direction …`, which returns the new terminal
  (`macos/Ghostty.sdef:192-201`), plus focus, close, input and
  `perform action`. AppleScript `close` uses `withConfirmation: false`
  (`macos/Sources/Features/AppleScript/ScriptTerminal.swift:189`). The App
  Intent "New Terminal" takes `splitLeft|Right|Up|Down` locations
  (`macos/Sources/Features/App Intents/NewTerminalIntent.swift:127-135`).
  Gated by `macos-applescript = true` (`src/config/Config.zig:3594`) and
  `macos-shortcuts = ask` (`:3692`).
- **Chrome.** `unfocused-split-opacity = 0.7`, clamped 0.15-1
  (`Config.zig:1068`, `:4922`); `split-divider-color` (`:1083`); no
  per-pane header.
- No agent model. It is the UX reference, not the agent-control reference.

### Zellij (MIT)

- `zellij action` covers write, send-keys, dump-screen, subscribe,
  list-panes, new-pane, move-pane, resize and close. `new-pane --no-focus`
  places the pane "relative to the pane the command was issued from"
  without changing any client's focus (`zellij-utils/src/cli.rs:1561-1565`).
- **Default target is the focused pane for most verbs.** `ZELLIJ_PANE_ID`
  is injected per pane (`zellij-server/src/os_input_output_unix.rs:249`),
  but only `set-pane-color` and `set-resurrection-command` default to it
  (`zellij-utils/src/input/actions.rs:2459-2470`, `:2490-2500`). This is
  the counterexample: a focused-pane default is unsafe for agents.
- **The most complete attention model.** Per-pane `AttentionStatus` (idle,
  working, new_output, quiet, done, failed, needs_you;
  `zellij-utils/src/attention.rs:11-20`), nine sources including OSC 7501,
  rollups from pane to tab to session to global, and mute scopes.

### WezTerm (MIT) and tmux (ISC)

- **WezTerm.** `wezterm cli split-pane` prints the new pane id and takes
  `--pane-id`, a side, `--cells|--percent` and `-- prog`. The target
  resolves from the flag, then `$WEZTERM_PANE`, then the most recently
  active client's focused pane (`wezterm-client/src/client.rs:1314-1330`).
  No wait verb. `kill-pane` acts "immediately and without prompting".
  `move-pane-to-new-tab` keeps the pane id. Inactive panes dim through
  `inactive_pane_hsb`.
- **tmux.** `TMUX_PANE=%N` (`spawn.c:415`). `split-window -d -P -F
  '#{pane_id}'` is the canonical "split, don't focus, print the id".
  `break-pane` and `join-pane` move a pane between windows with its id
  kept. `pane-border-status` gives a per-pane header. `window-size
  largest|smallest|latest|manual` is the classic answer to "whose client
  sizes a shared layout", which DL-31 rejected for tabs.

### bonsplit (MIT)

cmux's SwiftUI/AppKit split library. No agent API of its own. Relevant for
its veto hooks (`shouldSplitPane`, `shouldClosePane`: every close passes one
policy point), per-tab badge fields, and normalized bounds with stable ids
in its layout snapshot. AppKit-only, so the code is not reusable.

### Lessons for Roost (inference unless marked)

1. **The leaf terminal is the agent unit everywhere** (verified). Status,
   notification, "seen", read and wait attach to the leaf; containers only
   roll up. Roost's leaf is already the tab.
2. **Put only the per-terminal id in the environment.** A container id in
   env goes stale when the pane moves; herdr documents exactly that.
   Resolve the container on demand.
3. **Default to the caller, never to "focused".** Roost's `--tab` fallback
   already does this; every layout op keeps it.
4. **Automation never moves focus unless the verb is a focus verb** (cmux
   policy, herdr default).
5. **Return ids; never make the caller predict them** (herdr, tmux `-P
   -F`, WezTerm).
6. **Compute the new leaf's grid before spawning** (herdr).
7. **Prompting is not raw input.** Refuse to type into a blocked agent
   (herdr `agent_blocked`).
8. **Close safety is a gap in every reference.** Roost can do better
   cheaply by recording who opened a tab.
9. **Store normalized ratios, not cells.** Roost has clients of different
   sizes on one session.
10. **Make layout a pure function of a model.** Ghostty's GTK code calls
    its ratio sync with GtkPaned "a hack"; iced lets Roost avoid that.

---

## Candidate shapes against the goals

| | A: panes inside a tab | **H: session-owned layout of tabs** (G made concrete) | B: client-side tree | C2: client-side 2-up | C3: pin across projects |
|---|---|---|---|---|---|
| 1. Agents control panes | Yes | **Yes** | Only through the local window; not from remote hosts (#560), not with the window closed | Same as B, two slots | Same as B |
| 2. Parallel agents in one window | Yes | **Yes** | Yes | Two only | Two only |
| 3. Terminal beside an agent, everywhere | Yes | **Yes** | This window only | This window only | This window only |
| Ghostty/herdr-like UX | Yes | **Yes** | Mostly (strip shows tabs, not splits) | Partly | Partly |
| `ROOST_TAB_ID`, hooks unchanged | **No** (migration) | Yes | Yes | Yes | Yes |
| Session protocol | Bump to 8 | Additive | None | None | None |
| Mobile sees it | Must render or flatten | Sees adjacent tabs; later a switcher | No | No | No |
| Complexity | XL | L + L (two plans), then M | L | M | M |

### A: panes inside a tab

- If `ROOST_TAB_ID` names the container, every installed hook reports
  agent state for a container; two agents in one tab collide.
- If a new `ROOST_PANE_ID` names the terminal, every per-terminal op
  (`tab.write`, `tab.dump`, `tab.agent_report`, `tab.send_file`,
  `tab.take_size`, the data-plane attach, the tab events, the opener)
  retargets.
- A protocol-7 peer cannot meaningfully refuse a tab holding several
  terminals, so it bumps to 8 under the rule at `docs/reference/ipc.md:4857-4862`.
  Clients check `session_protocol` for equality
  (`crates/roost-client/src/state.rs:235`), so desktop, mobile and session
  move in lockstep.
- Amends DL-7, DL-13, DL-14, DL-18, DL-29 and DL-31.
- **Buys nothing H does not** for any of the three goals.

### B and C2: client-side views

- The layout is window state, like host tab selection. UI-socket ops only.
- From a local session tab an agent can still reach these ops while the
  window runs (`docs/reference/cli.md:65-95`). It cannot from a remote host
  (#560) or when the window is closed, and no other client sees the result.
- **They do not meet goal 1 or 3, and C2 does not meet goal 2.** They stay
  valid as a separate, later idea: C3, "pin an agent from another project
  beside me". H does not preclude it.
- C2 was this note's earlier first step. It is no longer needed as a UX
  probe: H's first plan (in-process only) tests the same UX with no attach
  or size-ownership change.

### H: a session-owned layout of tabs (recommended)

A **layout** is a workspace record that groups two or more tabs of **one
project** into a binary split tree. A tab in no layout is a strip item on
its own, exactly as today. It is option G with these choices made:
per-client focus and zoom, contiguous leaves, additive wire.

---

## H in detail

### Glossary

| User sees | Wire / code | Notes |
|---|---|---|
| **pane** | `tab` (`Tab`, `tab_id`, `TabKey`) | One terminal, one PTY. `ROOST_TAB_ID` is "your pane". |
| **tab** (a strip pill) | a lone `tab`, or a layout | A pill shows one tab or one layout. |
| split, divider | layout node `{axis, ratio, first, second}` | Ratio normalized 0-1, never cells. |

The container needs a name that does not collide with `RestoreLayout`:
rename the internal type, or call the new entity `split` or `pane_group`.
This note says "layout" for readability.

### Model and invariants

- `Inner` gains `layouts: BTreeMap<i64, LayoutRow>` beside `tabs`.
- `LayoutRow { id, project_id, root: Node }`, where `Node` is `Leaf(tab_id)`
  or `Split { id, axis: Row|Column, ratio, first, second }`.
- Layout ids come from `alloc_id`, so no id ever names two kinds of thing.
  They travel as `string_int64`.

Invariants, each enforced inside the commit:

1. Every leaf is a live tab of `project_id`. A layout never spans projects
   or hosts.
2. A layout has at least two leaves. Closing down to one dissolves it; the
   survivor becomes a lone tab.
3. A tab is in at most one layout.
4. **A layout's leaves are contiguous in tab `position` order.** The strip
   shows one pill per layout at its first leaf's position. Old clients and
   the v0.1.0 phone, which read flat tab lists, see a layout's panes side
   by side in the strip.
5. Ratios are finite and clamped (herdr: 0.1-0.9). Each client enforces a
   minimum in cells at render time and does not write it back.

**Ordering.** A split is one commit: the leaf's `tab.opened`, a
`tabs.reordered` that places it next to its sibling, and the
`layout.changed` that adds it share one batch and one revision. #640 makes
a tab listed only after its PTY exists, so the layout insert rides that
post-spawn publish. No client ever sees a leaf naming a tab it cannot list.

### Identity and addressing

- **`ROOST_TAB_ID` is unchanged** and now means "your pane". Hooks,
  `agent-hook`, OSC 7501, `notify`, `wait`, `tab prompt` and `doctor` work
  as they are.
- **No `ROOST_LAYOUT_ID` env var.** A pane can move between layouts, and
  env is fixed at spawn. An agent asks `roostctl layout get`, which
  defaults to `--tab "$ROOST_TAB_ID"`.
- On a UI socket layout ids take the `h<host>.<id>` spelling, and under
  `local-backend = session` a bare id is rewritten to the slot's
  (`docs/reference/ipc.md:1471-1474`). Mutating `layout.*` ops join
  `is_mutating_op` (`crates/roost-engine/src/ipc.rs:1726`) and the DL-20
  routing `tab.reorder` already uses (`ipc.rs:3858-3876`,
  `UiRequest::HostTabReorder`).
- Ids are per process. A restart or backend switch re-mints them, as for
  tabs.

### Op set

Every surface (keybind, palette, menu, `roostctl`, Lua, agents) routes
through these.

| Op | Params (sketch) | Result | Sockets | Notes |
|---|---|---|---|---|
| `tab.open` + `split` | existing params + `split: {beside, direction: right\|down\|left\|up, ratio?}`; `beside` defaults to the caller's tab when `roostctl` runs inside one | `{tab, layout}` | UI + session | Reuses the opener (DL-32), `argv`, `title`, `activate`, and `cwd_from_tab` defaulting to `beside`. `TabOpenParams` is `deny_unknown_fields` (`crates/roost-ipc/src/messages.rs:369`), so an old server refuses `split` with `unknown-field` and opens nothing: a meaningful refusal, no bump. The new leaf spawns at its slot's grid. |
| `layout.get` | `{tab_id}` or `{layout_id}` | `{layout: {id, project_id, root}}`, or `null` for a lone tab | UI + session | On a UI socket it adds this window's view: `focused_tab_id`, `zoomed`. |
| `layout.resize` | `{layout_id, node_id, ratio}` or `{tab_id, direction, amount}` | `{layout}` | UI + session | Last writer wins. The UI previews a drag locally and sends once on release. |
| `layout.equalize` | `{layout_id}` | `{layout}` | UI + session | Ghostty `equalize_splits`. |
| `layout.swap` | `{a, b}` (same layout) | `{layout}` | UI + session | herdr `pane.swap`. |
| `layout.move` | `{tab_id, to: {beside, direction, ratio?} \| {alone: true}}` | `{layout?}` | UI + session | Join and break-out (tmux `join-pane` / `break-pane`). Same project only. The tab id is kept. |
| `layout.apply` | `{project_id, root}`; a leaf is `{tab_id}` (reuse) or `{cwd?, argv?, title?}` (spawn) | `{layout, tabs}` | UI + session | herdr's declarative form. One call sets up "agent \| diff over file browser". |
| `layout.zoom` | `{tab_id, mode: toggle\|on\|off}` | `{}` | **UI only** | View state, like `palette.*`. |
| `layout.focus_direction` | `{direction}` | `{tab_id}` | **UI only** | Neighbor from normalized rects in `roost-ui-model`. Moves focus, so it is a focus verb. |
| `tab.close` | unchanged | unchanged | both | Closing a leaf collapses its parent; the sibling takes the space. |
| `tab.reorder` | unchanged | unchanged | both | Must keep invariant 4. An order that splits a layout answers `invalid-param`, naming it. The UI moves a pill by sending its leaves together. |
| `tab.focus` | unchanged | unchanged | both | Focusing a leaf shows its layout with that pane focused. Still the one focus verb (with `layout.focus_direction`). |

### Agent API

**What an agent uses** (sketch, names to argue with):

```bash
# Open a git diff watcher beside me, without taking the user's focus.
out=$(roostctl split --right --json -- sh -c 'git diff --stat; exec $SHELL')
new=$(printf '%s' "$out" | jq -r .tab.id)

# Start a second agent below me and give it work.
roostctl split --down --json -- claude
roostctl tab prompt --tab "$id" "Review the diff in the right pane"

# Read the layout I am in.
roostctl layout get --json
```

- `roostctl split [--right|--down|--left|--up] [--beside ID] [--ratio F]
  [--activate] [--cwd DIR | --here] [--title T] [--hold] [--json] [-- argv]`.
  Inside a tab it defaults to `--beside "$ROOST_TAB_ID"` and to **no
  activation**. From a plain terminal or a keybind it activates.
- `roostctl layout get|resize|equalize|swap|move|apply|zoom`.
- Everything else is today's verbs on the pane's tab id: `tab send`,
  `tab dump`, `tab prompt`, `wait`, `events`, `tab close`.
- Like `tab open` today, `split` from a local session tab goes to the
  owning window when there is one, so the window computes the exact grid;
  otherwise to the session (`docs/reference/cli.md:65-95`).

**Rules** (the cmux focus policy and herdr's skill, made into contract
where cheap):

1. **No layout op moves focus.** Only `tab.focus`, `layout.focus_direction`
   and an explicit `--activate` change selection.
2. **Default target is the caller.** Every op that takes a tab falls back
   to `ROOST_TAB_ID`. Nothing falls back to "the focused pane".
3. **Ids come back in the result.** Agents never derive them from strip
   order.
4. **`tab prompt` refuses a blocked target** (`needs_input` with a
   permission or question kind) unless `--force`. Today it does not
   (`prompt.rs:160-175`).
5. **Record who opened a tab.** `tab.open` records `opened_by` (the
   caller's tab id, which `roostctl` sends from `ROOST_TAB_ID`). A
   `tab.close` or `layout.move` from inside a tab, on a tab it did not open
   and that is not its own, answers `not-owner` unless `force: true`. The
   user's own close from the UI is never gated. This is a guardrail against
   agent mistakes, not a security boundary: any same-UID process can claim
   any caller.
6. **The skill teaches the rest** (herdr's list): split your own pane,
   right if wide and down if tall; keep background work unfocused; close
   only what you opened; never stop the session.

**Claude Code teammates (optional).** A `roostctl tmux` shim, installed
only for a process launched through something like `roostctl claude-teams`,
never globally. It sets `TMUX` and `TMUX_PANE=%<tab_id>`, puts the shim
first on `PATH`, and maps the verbs from the strings list:

| tmux verb Claude uses | Roost op |
|---|---|
| `display-message -p '#{pane_id}'` | `%$ROOST_TAB_ID` |
| `split-window -d -P -F '#{pane_id}' -- cmd` | `tab.open` + `split`, no activation |
| `list-panes -F '#{pane_id}'` | `layout.get` |
| `select-pane -T name` | `tab.set_title` |
| `select-layout`, `resize-pane` | `layout.equalize`, `layout.resize` |
| `set-option` border styles | a per-pane accent, or ignored |
| `kill-pane` | `tab.close` (it opened the pane, so the owner check passes) |

Risk: a set `TMUX` changes how other programs behave (clipboard,
passthrough, and Claude's own terminal detection; `agent-integration.md`
infers Claude's OSC 7501 probe goes to tmux inside tmux). Scope it to the
launched process. The reverse risk exists today: Roost does not scrub an
inherited `TMUX` from its own environment, so a Roost started from inside
tmux would hand teammates to the outer tmux (inference from the grep; not
run).

### Events and wire

- **`layout.changed`**: `{project_id, layout_id, layout | null}`, the whole
  tree (it is small). **Replayed**, because it is state, on the same
  revision and replay ring as the workspace events, so a resume stays
  consistent.
- **`tab.list`**: each `Project` gains `layouts`. `Project` is not
  `deny_unknown_fields` (`messages.rs:146-157`), so it is additive.
- **No `layout_id` on `Tab`.** One source of truth.
- Old mirrors ignore the new event: `roost-client`'s mirror drops unknown
  names (`crates/roost-client/src/mirror.rs:131-134`).
- `Tab` gains an optional `opened_by`.

### Agent state rollup and notifications

- **Per pane: unchanged.** Each leaf is a tab with its own axes
  (DL-13, DL-14, DL-29).
- **Strip pill for a layout:** `project_rollup` over its leaves, the same
  order as herdr's priority.
- **Project row and Agents panel: unchanged.** One row per agent tab. A
  click focuses that pane, which shows its layout.
- **"Being watched".** Two choices:
  - *Focused pane only* (today's rule): no engine change. A visible but
    unfocused pane still raises an OS banner while the user sits in the
    window.
  - *Every visible pane* (herdr): visible panes suppress the OS banner,
    and the pending bit plus an in-chrome **ring on the pane** stay until
    focus. Needs a `set_visible_tabs` input on the in-process workspace;
    for host tabs suppression is already client-local (#474), so it is an
    iced change only.

### Size ownership with several clients (DL-31)

- **Each leaf keeps its own owner.** A leaf is a tab, so per-tab
  ownership, advisory grids and the grace period are untouched.
- **Each client computes leaf grids itself** from the shared tree, its own
  window rect and each pane's own cell metrics.
- **Proposed amendment, the "group take".** A deliberate interaction with
  a split tab takes every pane it shows: an explicit switch to the pill, a
  window resize while focused, a divider drag. Typing, paste, clicks and
  the wheel take only the pane they land in. Without it, a laptop
  switching to a layout last sized by a desktop draws N letterboxed panes.
  On the wire it is N takes; no new frame.
- **Mixed ownership is legitimate.** A phone showing one pane takes that
  pane only; the desktop draws it "Sized by phone" in its rect.
- **Splits with no window attached.** When an agent splits on a session
  nobody is viewing, the engine halves the target's current grid along the
  axis (minus one cell for the divider). If nobody owns the target, it
  resizes it too. If someone does, that client re-grids on
  `layout.changed`; a control-plane resize of an owned tab is refused by
  design.
- **Connection budget.** Four panes on three clients, plus #566's warm
  tabs, approach the cap of 32. Raise it or cap panes per layout.

### Protocol and compatibility

- **Additive throughout:** new ops, a new event name, new optional fields,
  and a new optional `tab.open` field that old servers refuse with
  `unknown-field`. Under the rule at `docs/reference/ipc.md:4857-4862` none
  of this bumps `SESSION_PROTOCOL_VERSION = 7` (`messages.rs:2722`).
- **New client, old session.** `session.identify.ops` lacks `layout.*`, so
  the client hides split actions for that host. But two documents say
  this is not yet a contract: "the contents of `ops` are not a
  compatibility promise" (`docs/reference/ipc-compatibility.md:252-254`)
  and "There is no intra-generation capability channel today"
  (`ipc.md:4862`). The sessions plan amends one, or resurrects the
  `features` channel the compatibility page describes.
- **Old client, new session.** It sees a layout's panes as adjacent plain
  tabs and can type into, close, focus and take any of them. Its
  `tab.reorder` that would split a layout is refused, and so is the
  reorder half of an old `roostctl tab open --after-tab` into the middle
  of a layout (the tab still opens, at the end). Nothing it does corrupts
  a layout.
- One-back compatibility kept at v0.1.0 is unaffected.

### Persistence

- `ProjectSnapshot` gains `layouts`, omitted while empty. Leaves refer to
  **positions** in `tabs`, because ids are not stable across restore. An
  older build ignores the key and restores flat tabs.
- Restore re-opens tabs by position, then relinks layouts. A leaf whose
  restore fails (the #559 class, closed) is dropped and its parent
  collapses.
- The session's `state.json` uses the same code.
- Zoom and, for host projects, the focused pane go beside `HostTabMemory`.
  In-process projects already restore the active tab by position.
- Commands are not restored, as today (DL-7): a relaunch re-opens shells
  in the panes' directories. A "resume command" per pane (herdr's
  `resume_argv`, Zellij's resurrect) is a later question.

### Native chrome (iced)

Colors from `ChromePalette`; sizes from the chrome font-size seam in
`chrome-themes.md`.

- **Pane header strip**, only when a pill holds two or more panes: title
  (program or cwd basename), the agent lifecycle dot in the strip's colors,
  the "Sized by X" badge, a close button. Ghostty has no header; herdr
  titles its borders; tmux has `pane-border-status`.
- **Focused pane:** accent header and a thin accent border. Unfocused
  panes dimmed, as an `unfocused-split-opacity` key (Ghostty's 0.7,
  clamped 0.15-1).
- **Attention ring** on an unfocused pane whose agent waits (cmux's ring,
  design level).
- **Optional progress bar** from OSC 9;4 or the 7501 progress field.
- **Pill:** focused pane's title, a pane-count glyph, the rollup dot.
- **Divider:** the `agents_split` pattern with an epoch; local preview
  during the drag, one `layout.resize` on release (avoids herdr's
  fish-repaint-on-resize class). Double-click equalizes.
- **Keybinds** (new `KeybindAction` variants; defaults to check):
  `SplitRight`, `SplitDown`, `FocusPane{dir}`, `ZoomPane`,
  `EqualizeSplits`, `ResizePane{dir}`, `BreakPaneToTab`. ⌘W closes the
  focused pane. macOS `super+d` / `super+shift+d` match Ghostty and cmux.
  On Linux the primary modifier is `alt`, and `alt+d` collides with
  readline; Ghostty's GTK `ctrl+shift+o` / `ctrl+shift+e` are free in Roost
  but COSMIC's own bindings need checking.
- **Palette, menu bar, context menu:** one row per action; "Split right
  with new shell" and "Move to own tab" in the tab context menu
  (`ContextAction`, `crates/roost-ui-model/src/context_menu.rs:28-44`).

### iced work

- **Geometry is a pure function in `roost-ui-model`:** tree, content rect,
  per-pane cell size, divider width and minimum cells in; per-leaf rect
  snapped to whole cells and `(cols, rows)` out. Unit-tested like
  `agent_panel.rs`. It replaces the window-wide `current_grid` and makes
  `regrid_window` per leaf. Hidden layouts are sized the same way so a
  switch does not reflow them.
- **View.** One split widget whose children are one `TerminalWidget` per
  leaf, in a stable order (by `TabKey`; ids only grow, so a split appends)
  or with keyed diffing. Zoomed-out panes at zero size, not removed.
- **Focus.** The focused pane is the active tab, so the keyboard route is
  unchanged. The cursor-focus flag gains a tab key; IME stays on the
  focused pane only.
- **Pointer.** `on_screen` becomes the set of shown leaves; the reconcile
  cancels for leaves leaving it. A press in an unfocused pane focuses it;
  whether it is also delivered is a key question (WezTerm delivers by
  default; the risk is a click landing in an agent's permission menu).
  File drop targets the pane under the pointer.
- **Attach (sessions plan).** `host_attach` holds every leaf of the shown
  layout; DL-18 becomes "attach on show". This is #566's plumbing.
- **Notice and badge** move from the tab band to each pane.
- **Render cost.** N panes multiply per-frame cell draws (#309, open).
  Not measured.

### Mobile

- **The v0.1.0 app, unchanged:** a layout's panes show as adjacent chips
  (`mobile/lib/hosts/tab_strip.dart`), one attached at a time. Usable as
  is.
- **Later plan:** `roost-client` exposes layouts; the strip groups a
  layout's chips into one chip with a pane switcher; the phone still shows
  **one pane at a time** and takes only that pane (DL-34's rule). Split
  actions are plain ops, so the phone can start a "diff beside agent"
  layout for the desktop.
- **Much later:** a tablet 2-up with two attach handles (L).

### Testing

- **Engine unit tests:** split, close-collapse, dissolve at one leaf,
  ratio clamp, swap, move (id kept), apply, contiguity under
  `reorder_tabs`, project delete cascading, `layout.changed` in the same
  batch and revision as `tab.opened`, replay and resume, snapshot
  round-trip, restore with a failed leaf, `opened_by` close refusal.
- **`roost-ui-model`:** geometry snapping and minimums, mixed cell sizes,
  directional neighbor, pill rollup, keybind and palette rows.
- **Wire:** `tests/ipc-vectors/` per op and event, the schema bundle, and
  a vector for `tab.open.split` refused `unknown-field`.
- **roosttest (functional):** `split` then `layout get` and `tab list
  --json`; an additive `panes: [{tab_id, left, top, width, height, cols,
  rows}]` in `app.window_metrics`, beside the existing `terminal_left` /
  `terminal_top` that about 137 lines in 26 `tools/` files read; `tab.dump`
  per pane shows its own cols; relaunch restore.
- **The agent flow, end to end:** from inside a pane, `roostctl split
  --json -- sh -c …`, `tab prompt` to the sibling, then assert the user's
  selection did not move; a prompt to a `needs_input` sibling is refused;
  a close of a pane the caller did not open is refused.
- **Session lane:** two clients, the group take on switch, the phone-style
  single-pane take, `host.size_trace` per leaf; an old-client lane that
  ignores `layouts`.
- **Real input:** divider drag and click-to-focus through the Mac and
  Linux harnesses (`tools/input/mac/coords.py` learns per-pane origins).
  One screenshot shot of a three-pane layout.
- **A negative control on every new test.**

---

## Roadmap

| Plan | Scope | Out | Depends on | Size |
|---|---|---|---|---|
| **P0: decide** | A DL entry choosing H (it supersedes DL-35's "under discovery"); the container's name; whether agents get the ownership guard | — | — | S |
| **P1: agent-control groundwork** | `tab prompt` refuses a blocked target unless `--force`; `opened_by` on `tab.open` and the `not-owner` close guard; atomic placement on `tab.open` (server-side, replacing the CLI's open-then-reorder for `--after-tab`); skill rules. Useful today for parallel agents in separate tabs, and the base for splits. | Layouts | — | S-M |
| **P2: engine and in-process splits** | Layout model, invariants, ops, `layout.changed`, persistence; `tab.open` + `split`; `roostctl split` and `layout`; ui-model geometry and neighbor; iced split widget, per-pane grid, focus, cursor, pointer, notice, header, accent, divider, zoom; `window_metrics.panes`; keybinds, palette, menu, context menu; focused-only suppression | Session and host projects (iced shows split actions only for in-process projects); `layout.apply`; mobile | P1 | L (upper) |
| **P3: sessions and hosts** | The session serves the ops; `roost-client` and the iced host mirror apply `layout.changed`; DL-20 routing for host-qualified layout ids; attach-on-show for every leaf; the DL-31 group take; per-leaf background wave; no-window spawn halving; connection cap; capability detection; old-client tests. Fresh installs run `local-backend = session` (DL-27), so this is what most new users need. | Mobile UI | P2; #566 or its plumbing | L |
| **P4: agent-driven layouts** | `layout.apply` and `roostctl layout apply <file>`; config layout presets with palette "Apply layout…" ("agent \| git diff over file browser"); skill recipes (sibling pane, second agent beside you, diff or browser beside the agent); visible-pane suppression and the attention ring; progress bar | tmux shim | P3 (remote-host agents split through their session) | M |
| **P5: Claude Code teammates (optional)** | A spike first: run Claude Code with a stub tmux under an isolated `HOME` and `CLAUDE_CONFIG_DIR`, log the exact verbs and formats it needs. Then the scoped shim and a launcher. | A general tmux emulation | P3 | spike S, then M (inference; grows if Claude needs more than the strings show) |
| **P6: mobile** | Layout-aware strip with a pane switcher; one pane at a time; split actions | Tablet 2-up (L, later) | P3 | M |

- **Why P2 excludes sessions.** It tests the UX with no attach or
  size-ownership change, because in-process tabs have neither. P3 should
  follow closely, because the session backend is the fresh-install default.
- **Total.** Two L plans plus M and S-M ones: XL across the whole track.
- **Where #566 fits.** It is independently useful (faster switch-back) and
  provides the multi-attach plumbing P3 needs. Landing it before P3 shrinks
  P3.

---

## Risks

- **Strip semantics.** A pill that is a layout changes ⌘1-9, reorder,
  `last_tab_position`, `JumpToUnread` and close-last-tab rules. Each needs
  a stated meaning and a test.
- **The group take is new DL-31 policy.** It is easy to get a race where
  two clients keep re-taking panes. Needs its own DL entry and session lane.
- **iced tree identity.** A wrong child order silently moves a pane's
  widget state (selection, scroll, gesture) to another pane.
- **The "on screen == active" assumption** is spread over about 30 call
  sites; each needs review.
- **Render cost** of N panes (#309), not measured.
- **Agents fighting the user.** Without the focus rule and the ownership
  guard, an agent can close or resize what the user is looking at. Both
  are in P1 for that reason.
- **Prompting into a permission dialog.** Today's `tab prompt` would
  (verified). P1 fixes it before agents drive each other.
- **tmux shim fidelity.** Claude Code may need tmux behavior beyond the
  verbs seen in strings. A partial shim fails in ways that are hard to
  see. Hence the spike.
- **Inherited `TMUX` / `CLAUDE*` in the server's own environment** (not
  scrubbed today) would mislead agents in every pane (inference).

---

## Recommendation

1. **Build H, not literal A and not a client-only split.** H meets all
   three goals, keeps `ROOST_TAB_ID` and hooks, needs no protocol bump,
   and puts layouts where agents, remote hosts and the phone can reach
   them.
2. **Structure shared, view per client.** Tree and ratios are workspace
   state; the focused pane and zoom are window state. This keeps DL-26
   ("there is no lease") and host selection as client state.
3. **Ship the agent-control groundwork first (P1).** It is small, useful
   for parallel agents in tabs today, and makes the later layout ops safe
   for agents from day one.
4. **Agents split without focus by default**, target the caller, get ids
   back, and close only what they opened.
5. **Amend DL-31 with the group take** in P3, as its own entry with a test.
6. **Treat the Claude Code tmux shim as optional**, after a spike. The
   native `roostctl split` plus the skill covers every agent first.
7. **Keep C3 (pin across projects) as a separate, later idea.** H does not
   block it.

---

## Key questions

1. **Naming.** "Layout", "split" or "pane group" for the container, given
   `RestoreLayout`? In user docs, is it "panes" inside a "tab", as in
   Ghostty and herdr?
2. **Can a layout span projects?** H says no, so the strip, persistence
   and rollup stay per project. If side-by-side agents in *different*
   projects matter, that is C3, client-side.
3. **Visible-pane suppression.** Should a visible but unfocused pane
   suppress the OS banner and show a ring instead?
4. **Ownership guard.** Should `opened_by` gate `tab.close` from inside
   tabs, or is herdr's skill-only rule enough?
5. **Prompt refusal.** Should `tab prompt` refuse every `needs_input`
   target, or only a blocked permission or question (program status kind)?
6. **Agent focus.** Should `roostctl split` from inside a pane default to
   no activation while keybinds activate? May an agent ever focus a pane
   without the user asking?
7. **Group take.** Should an explicit switch to a split tab take every
   pane? Should a divider drag take both sides?
8. **Click into an unfocused pane:** focus only, or focus and deliver?
9. **Zoom:** per client (proposed) or shared? Does focusing another pane
   while zoomed keep the zoom (Ghostty `split-preserve-zoom`)?
10. **Pill title and rename:** follow the focused pane (Ghostty), or add an
    optional layout title?
11. **Limits:** maximum panes per layout, minimum pane size in cells, and
    raise `MAX_DATA_CONNS_PER_SESSION` or cap panes?
12. **⌘W and close-last-tab:** close the focused pane, with a separate
    "close tab" that closes the whole layout?
13. **Capability detection:** amend `ipc-compatibility.md` so `ops` may
    gate features, or resurrect the `features` channel?
14. **Claude Code teammates:** worth a spike and a scoped shim, or is a
    Roost-native `split` plus the skill enough?
15. **Restore commands:** should a layout preset or a relaunch re-run each
    pane's command (herdr `resume_argv`, Zellij resurrect), or only shells
    in the right directories as today (DL-7)?
16. **Floating or scratch panes** (Zellij) for a quick file browser: ever
    in scope?
17. **Multi-window** stays a non-goal? H does not need it.

---

## Verified and inferred

**Verified at `68231b9b` by reading code and docs:** every Roost
file:line above; the non-goal's history; `Inner`, `commit`, `alloc_id`,
`reserve_tab`, `publish_tab`, `reorder_tabs`, `TabsReordered`,
`RestoreLayout`; the #640 comments; the env injection and the opener's
four names; the absence of `CLAUDE*` / `TMUX` scrubbing in `pty.rs`;
`TabOpenParams` `deny_unknown_fields`; `Project`'s shape; the op list;
`SESSION_PROTOCOL_VERSION = 7` and the client's equality check; the mirror
ignoring unknown events; `is_mutating_op`, the `tab.reorder` host routing,
the 32-connection cap and its doc; `tab open --after-tab` as a separate
reorder; `tab prompt` writing whatever the target's state; `wait`'s
options; the persistence structs; `KeybindAction` and per-tab font size;
`ContextAction`; `split_handle`; `project_rollup`; the iced view, focus
and grid functions; iced's positional `diff_children`; the bump rule and
the capability sentences; the inside-a-session-tab routing. Issue states:
#560, #566, #518, #630, #480, #309 open; #474, #503, #559, #640 closed.

**Verified in the references:** herdr's split params and focus default,
env removal, ratio clamp, `agent_blocked`, rollup priority, close handler
and skill rules; Zellij's `ZELLIJ_PANE_ID` injection, its two env-default
verbs, `--no-focus` and `AttentionStatus`; WezTerm's pane resolution;
tmux's `TMUX_PANE`; Ghostty's `split` command, `close` without
confirmation, surface env and split config; crush reading
`HERDR_PANE_ID` and sending `pane.report_agent`; the Claude Code strings
listed above; cmux's README and CLI-contract lines.

**Inferred, not run:** that H needs no bump in practice (it follows the
documented rule; nothing was built); the group-take UX; the spawn-halving
behavior; every complexity grade; the render cost of N panes; the tmux
shim's fidelity; how a Roost started inside tmux would affect Claude's
teammate backend.

---

## References

| Repo (`/home/charliek/projects/thirdparty/`) | Head | License | Use |
|---|---|---|---|
| `herdr` | `ee5bfb5` | Apache-2.0 | In depth: `skills/herdr/SKILL.md`, `src/api/schema/panes.rs`, `src/app/api/panes.rs`, `src/app/api/agents.rs`, `src/pane.rs`, `src/layout.rs`, `src/workspace/aggregate.rs`, `src/app/actions.rs`, socket API docs |
| `ghostty` | `eb895f612` | MIT | Split tree, `Ghostty.sdef`, AppleScript and App Intents, surface env, split config |
| `zellij` | `43c8710` | MIT | `cli.rs` actions, `input/actions.rs` defaults, pane env, attention model |
| `wezterm` | `458a100` | MIT | `wezterm/src/cli/*`, `wezterm-client/src/client.rs` pane resolution, `mux/src/tab.rs` |
| `tmux` | `a73616d` | ISC | `spawn.c`, `cmd-find.c`, `cmd-break-pane.c`, `resize.c`, options |
| `bonsplit` | `51d924b` | MIT | Controller and delegate API, badge fields, layout snapshot |
| `crush` | `8f1f0295` | FSL-1.1-MIT | **Design level.** `internal/herdr/client.go`: reports agent state to herdr by `HERDR_PANE_ID` |
| `alacritty` | `aa5d942` | Apache-2.0 / MIT | README stance: splits are "best left to a window manager or terminal multiplexer" |
| `cmux` | `a79cbde73ba` | GPL-3.0-or-later (server dirs BUSL-1.1) | **Design only.** README, `docs/cli-contract.md`, `docs/cli-command-tree.txt`, `docs/socket-focus-steal-audit.todo.md`, `docs/configuration.md`, `skills/cmux-socket-policy/SKILL.md`. No code read, quoted or paraphrased. |
| `cosmic-term` | `9129277` | GPL-3.0 | **Design only.** A pane grid with per-pane tabs; no code read |
| Claude Code 2.1.319 | installed binary | proprietary | `strings` only: `teammateMode` values and tmux verbs |
| iced 0.14 (cargo registry) | `iced_core-0.14.0`, `iced_widget-0.14.2` | MIT | `widget/tree.rs` positional diff, `keyed`, `pane_grid`, `InputMethod::merge` |

Related Roost notes: `chrome-themes.md` (chrome font size, palette),
`agent-integration.md` (hooks through the environment, OSC 7501, Claude's
probe inside tmux), `host-sessions.md`, `sessions-everywhere.md`,
`protocol-7-release.md` (DL-31 rules).
