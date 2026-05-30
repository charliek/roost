# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/). Releases are
cut with `/release:release vX.Y.Z` — it curates the section below, commits, tags
`vX.Y.Z`, and pushes; the tag triggers `.github/workflows/release.yml`, which
builds the DMG + `.deb`s and publishes to the apt repo. Bump
`[workspace.package].version` in `Cargo.toml` to match before tagging (the
release workflow asserts they agree).

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
