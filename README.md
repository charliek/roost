# Roost

A macOS + Linux desktop terminal multiplexer for AI coding agents (Claude Code,
Codex, and the like). Sidebar of projects on the left, tabs per project, one
libghostty-vt terminal per tab. The
`roostctl` companion CLI surfaces notifications when an agent in a tab needs
attention.

One native UI — **Rust + iced**, on macOS (`Roost.app`) and Linux
(`roost`) — embeds the workspace + PTY supervisor + a JSON-IPC server
**in-process**. A fresh install starts its local tabs on a `roost-session` it
manages; the headless `roost-session` daemon is also what powers
[host sessions](docs/guides/host-sessions.md) — shells that keep running
after the window closes. External
tooling (`roostctl`, Claude Code hooks) talks to the running UI over
newline-delimited JSON on a Unix-domain socket; the wire contract is in
[`docs/reference/ipc.md`](docs/reference/ipc.md).

## Install

**Linux (Ubuntu 24.04+ / Pop!_OS 24.04+)** — via the apt repo:

```bash
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://apt.stridelabs.ai/pubkey.gpg | sudo tee /etc/apt/keyrings/apt-charliek.gpg > /dev/null
echo 'deb [signed-by=/etc/apt/keyrings/apt-charliek.gpg] https://apt.stridelabs.ai noble main' | sudo tee /etc/apt/sources.list.d/apt-charliek.list
sudo apt update
sudo apt install roost          # installs the `roost` UI + the `roostctl` CLI
```

**macOS** — download `Roost-<version>.dmg` from the
[latest release](https://github.com/charliek/roost/releases/latest), open it,
and drag `Roost.app` to Applications. Release DMGs are
Developer-ID signed and notarized by Apple, so Roost opens with a normal
double-click — no Gatekeeper detour. See
[Installation](docs/getting-started/installation.md#shipping-builds)
for details.

Coming from `Roost-Iced.app`? See
[Roost-Iced.app users](docs/getting-started/installation.md#roost-icedapp-users).

### Claude Code / agent skills

Roost ships a skill that teaches your coding agent to drive a running Roost
through `roostctl`: open a project tab running a command, send input, read a
tab's screen, wait on its state, watch events, and notify you. Install Roost
(above) first, since the skill drives it; `roostctl skill` prints the same
skill.

The general route ([`skills`](https://skills.sh)) installs into Claude Code,
GitHub Copilot, OpenCode, and other agents:

```bash
npx skills add charliek/roost
```

For Claude Code, a native plugin is also available (it namespaces the skill as
`roost:roost`):

```text
/plugin marketplace add charliek/roost
/plugin install roost@roost
```

## Build from source

```bash
git clone https://github.com/charliek/roost
cd roost
mise install                          # Rust (rust-toolchain.toml) + Zig 0.17.x
./third_party/ghostty/build.sh        # clones Ghostty at the pinned SHA, builds libghostty-vt

# The iced UI — the only desktop app (Linux needs: sudo apt install libclang-dev pkg-config):
cargo build --release -p roost-iced -p roost-cli    # → target/release/{roost-iced,roostctl}
./linux/scripts/build-deb.sh 0.0.1-dev              # …or build an installable .deb (packages roost-iced as /usr/bin/roost via the linux-package feature; also needs nfpm on PATH)

# macOS app bundle (dev build, → mac/build/Roost-Iced.app):
make bundle-iced
```

## Documentation

The full site lives under `docs/` and builds with [Zensical](https://zensical.org) (`make docs-serve` → http://127.0.0.1:7070):

- [Installation](docs/getting-started/installation.md) — toolchain + build
- [First Run](docs/getting-started/first-run.md) — launch behavior + where state lives
- [Keybindings](docs/getting-started/keybindings.md) — tab/project switching, clipboard, mouse, scrollback
- [Working Directory Tracking](docs/guides/cwd-tracking.md) — header + tab labels follow `cd` (auto-loaded for zsh + modern bash)
- [Notifications](docs/guides/notifications.md) — how `roostctl` + OSC fallbacks surface in the UI
- [Claude Code Hooks](docs/guides/claude-code.md) — copy-paste `settings.json`
- [Host Sessions](docs/guides/host-sessions.md) — the opt-in `roost-session` daemon: shells that survive closing the window
- [Architecture](docs/reference/architecture.md) — package layout + threading contract
- [Vision & Principles](docs/development/vision.md) — what Roost is for, the north star, principles, directions under discovery, and the decision index ([full decision history](docs/development/decision-history.md))
- [IPC Reference](docs/reference/ipc.md) — the JSON wire format `roostctl` and Claude hooks speak

`CLAUDE.md` at the repo root captures the project conventions enforced by review.
