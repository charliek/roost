# Roost

A macOS + Linux desktop terminal multiplexer for AI coding agents (Claude Code,
Codex, and the like). Sidebar of projects on the left, tabs per project, one
libghostty-vt terminal per tab. The
`roostctl` companion CLI surfaces notifications when an agent in a tab needs
attention.

Two native UIs — **Swift + AppKit on macOS** (`Roost.app`) and **Rust + iced
on Linux** (`roost`) — each embed the workspace + PTY supervisor + a JSON-IPC
server **in-process** (no daemon). macOS also gets an experimental
**Roost-Iced.dmg**, the same iced UI built for Mac, installed side by side
with `Roost.app` — see the
[latest release](https://github.com/charliek/roost/releases/latest). External
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
and drag `Roost.app` to Applications. Release DMGs (v0.0.18 onward) are
Developer-ID signed and notarized by Apple, so Roost opens with a normal
double-click — no Gatekeeper detour. See
[Installation](docs/getting-started/installation.md#shipping-builds)
for details.

An experimental **`Roost-Iced-<version>.dmg`** is also published on the same
release — the iced UI built for macOS, installed side by side with
`Roost.app` under its own bundle id. See
[Installation](docs/getting-started/installation.md) for details.

## Build from source

```bash
git clone https://github.com/charliek/roost
cd roost
mise install                          # Rust (rust-toolchain.toml) + Zig 0.16.x
./third_party/ghostty/build.sh        # clones Ghostty at the pinned SHA, builds libghostty-vt

# Linux UI — iced, what the packaged .deb ships (needs: sudo apt install libclang-dev pkg-config):
cargo build --release -p roost-iced -p roost-cli    # → target/release/{roost-iced,roostctl}
./linux/scripts/build-deb.sh 0.0.1-dev              # …or build an installable .deb (packages roost-iced as /usr/bin/roost via the linux-package feature; also needs nfpm on PATH)

# macOS UI:
cd mac && swift build                 # or: ./mac/scripts/bundle.sh release  → mac/build/Roost.app
```

## Documentation

The full site lives under `docs/` and builds with [Zensical](https://zensical.org) (`make docs-serve` → http://127.0.0.1:7070):

- [Installation](docs/getting-started/installation.md) — toolchain + build
- [First Run](docs/getting-started/first-run.md) — launch behavior + where state lives
- [Keybindings](docs/getting-started/keybindings.md) — tab/project switching, clipboard, mouse, scrollback
- [Working Directory Tracking](docs/guides/cwd-tracking.md) — header + tab labels follow `cd` (auto-loaded for zsh + modern bash)
- [Notifications](docs/guides/notifications.md) — how `roostctl` + OSC fallbacks surface in the UI
- [Claude Code Hooks](docs/guides/claude-code.md) — copy-paste `settings.json`
- [Architecture](docs/reference/architecture.md) — package layout + threading contract
- [Vision & Principles](docs/development/vision.md) — direction, decision log, and the two-implementation architecture
- [IPC Reference](docs/reference/ipc.md) — the JSON wire format `roostctl` and Claude hooks speak

`CLAUDE.md` at the repo root captures the project conventions enforced by review.
