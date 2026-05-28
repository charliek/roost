# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/). Releases are
cut with `/release:release vX.Y.Z` — it curates the section below, commits, tags
`vX.Y.Z`, and pushes; the tag triggers `.github/workflows/release.yml`, which
builds the DMG + `.deb`s and publishes to the apt repo. Bump
`[workspace.package].version` in `Cargo.toml` to match before tagging (the
release workflow asserts they agree).

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
