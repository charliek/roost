# Installation

Roost is a Rust gRPC daemon (`roost-core`) paired with a native UI on each platform:

| Platform | UI | How it builds |
|---|---|---|
| macOS | Swift + AppKit (`Roost.app`) | SwiftPM via `mac/scripts/bundle.sh` |
| Linux | Rust + gtk4-rs (`roost-linux`) | `cargo build -p roost-linux` |

The daemon, the Linux UI, the companion CLI, and the `libghostty-vt` FFI all live in one Cargo workspace under `crates/`. The Swift UI is its own SwiftPM package under `mac/` and links the same vendored `libghostty-vt` static archive.

The CLI is named `roost-cli-rs` during the transition; it renames to `roost-cli` in the Phase 9 cutover. The legacy Go + GTK4 binary still ships from `main` — see [Legacy → Installation](../reference/legacy-go/installation.md) for that build path.

## Prerequisites

| Tool | Purpose | Pinned version |
|---|---|---|
| Rust | Daemon, CLI, Linux UI | 1.85.0 (via `mise`) |
| Zig | Builds `libghostty-vt` from the vendored Ghostty source | 0.15.x (via `mise`) |
| `protoc` | Generates Rust + Swift bindings from `proto/roost.proto` | any recent |
| Xcode Command Line Tools | Builds the Mac UI | macOS only |
| GTK4 + libadwaita dev packages | Linker dependencies for the Linux UI | Linux only |
| `mise` | Manages the pinned Rust + Zig versions | any |

## macOS

Install system packages:

```bash
brew install mise protobuf
```

Recommended: JetBrains Mono — the default font Roost looks for.

```bash
brew install --cask font-jetbrains-mono
```

Clone the repo and provision the toolchain:

```bash
git clone https://github.com/charliek/roost.git
cd roost
mise install
```

Build `libghostty-vt` once (idempotent on cache hit):

```bash
./third_party/ghostty/build.sh
```

Build the daemon and CLI:

```bash
~/.cargo/bin/cargo build --release -p roost-core -p roost-cli-rs
```

Bundle the Mac `.app`:

```bash
PROTOC_PATH=$(which protoc) ./mac/scripts/bundle.sh release
open mac/build/Roost.app
```

The daemon starts on demand the first time the UI connects.

### macOS 26 (Tahoe) `libghostty-vt` shim

`third_party/ghostty/build.sh` ships the same `arm64-macos` SDK shim as the legacy `build/build.sh`. When it detects macOS 26+ on Apple Silicon with an `arm64e`-only system SDK, it redirects Zig's SDK lookup to a sibling `MacOSX1[45].sdk` for the duration of the `zig build` call. Xcode Command Line Tools usually keeps one prior major SDK installed; reinstall (`xcode-select --install`) if you hit the `no sibling MacOSX1[45].sdk` error.

## Linux (Ubuntu / Debian)

System packages:

```bash
sudo apt update
sudo apt install -y \
  build-essential git curl pkgconf \
  libgtk-4-dev libadwaita-1-dev \
  protobuf-compiler
```

Recommended font:

```bash
sudo apt install -y fonts-jetbrains-mono
```

`mise` install (one-time, see the [official instructions](https://mise.jdx.dev/getting-started.html)):

```bash
curl https://mise.run | sh
echo 'eval "$(mise activate bash)"' >> ~/.bashrc
```

Clone and provision:

```bash
git clone https://github.com/charliek/roost.git
cd roost
mise install
```

Build `libghostty-vt`:

```bash
./third_party/ghostty/build.sh
```

Build everything. `roost-linux` requires the GTK4 + libadwaita system packages above; `cargo build` without `-p` skips it so contributors who only iterate on the daemon don't need GTK installed.

```bash
~/.cargo/bin/cargo build --release \
  -p roost-core -p roost-cli-rs -p roost-linux
```

Run the Linux UI:

```bash
~/.cargo/bin/cargo run --release -p roost-linux
```

## CLI on PATH

Install `roost-cli-rs` so it's reachable from any shell (Claude Code hooks call it without a full path):

```bash
sudo install -m 755 target/release/roost-cli-rs /usr/local/bin/roost-cli-rs
```

## Verifying the install

With the UI running:

```bash
~/.cargo/bin/cargo run --release -p roost-cli-rs -- identify
```

Prints a JSON object with the daemon socket path, PID, and active project / tab IDs. If you see a connection error, the UI isn't running or the socket path is wrong — see [Paths & Environment](../reference/paths.md).

## Updating

When the pinned Ghostty SHA changes, re-build `libghostty-vt`:

```bash
./third_party/ghostty/build.sh --force
~/.cargo/bin/cargo build --release
```

`--force` discards the cached Ghostty source tree and re-clones at the new SHA. After it finishes, the Mac UI's next `bundle.sh` run picks up the new archive automatically.
