# Roost

A macOS + Linux desktop terminal multiplexer for AI coding agents. Sidebar of
projects on the left, tabs per project, one libghostty-vt terminal per tab. The
`roostctl` companion CLI surfaces notifications when an agent in a tab needs
attention.

Two native UIs — **Swift + AppKit on macOS** (`Roost.app`) and **Rust + gtk4-rs
on Linux** (`roost`) — each embed the workspace + PTY supervisor + a JSON-IPC
server **in-process** (no daemon). External tooling (`roostctl`, Claude Code
hooks) talks to the running UI over newline-delimited JSON on a Unix-domain
socket; the wire contract is in [`docs/reference/ipc.md`](docs/reference/ipc.md).

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
[latest release](https://github.com/charliek/roost/releases/latest) and drag
Roost to Applications. (The DMG is currently ad-hoc-signed pending an Apple
Developer account, so first launch is right-click → Open.)

## Build from source

```bash
git clone https://github.com/charliek/roost
cd roost
mise install                          # Rust (rust-toolchain.toml) + Zig 0.15.x
./third_party/ghostty/build.sh        # clones Ghostty at the pinned SHA, builds libghostty-vt

# Linux UI (needs: sudo apt install libgtk-4-dev libadwaita-1-dev pkg-config):
cargo build --release -p roost-linux -p roost-cli   # → target/release/{roost,roostctl}
./linux/scripts/build-deb.sh 0.0.1-dev              # …or build an installable .deb

# macOS UI (needs: brew install gtk4 libadwaita):
cd mac && swift build                 # or: ./mac/scripts/bundle.sh release  → mac/build/Roost.app
```

## Documentation

The full site lives under `docs/` and builds with `mkdocs-material` (`make docs-serve` → http://127.0.0.1:7070):

- [Installation](docs/getting-started/installation.md) — toolchain + build
- [First Run](docs/getting-started/first-run.md) — launch behavior + where state lives
- [Keybindings](docs/getting-started/keybindings.md) — tab/project switching, clipboard, mouse, scrollback
- [Working Directory Tracking](docs/guides/cwd-tracking.md) — shell snippet so the header + tab labels follow `cd`
- [Notifications](docs/guides/notifications.md) — how `roostctl` + OSC fallbacks surface in the UI
- [Claude Code Hooks](docs/guides/claude-code.md) — copy-paste `settings.json`
- [Architecture](docs/reference/architecture.md) — package layout + threading contract

`CLAUDE.md` at the repo root captures the project conventions enforced by review.

## Legacy (Go)

The original Go + GTK4 implementation (`cmd/`, `internal/`, `go.mod`) is retained
for now and still builds (`mise install && make build` → `./roost ./roost-cli`),
but the Rust/Swift port above is the path forward. The Go code is slated for
removal once parity is confirmed — see [`plans/GODELETE.md`](plans/GODELETE.md).
Its docs live under the "Legacy (Go prototype)" section of the docs site.
