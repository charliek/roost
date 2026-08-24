# Installation

## Shipping builds

Most people want a released build, not a source checkout.

Linux (Ubuntu Noble / Pop!\_OS 24.04+) — add the `apt.stridelabs.ai`
apt repo once, then:

```bash
sudo apt install roost
```

For a one-off install without adding the repo, grab the `.deb` from
the [latest GitHub release](https://github.com/charliek/roost/releases)
and `sudo apt install ./roost_<version>_<arch>.deb`.

macOS — download `Roost-<version>.dmg` from the
[latest GitHub release](https://github.com/charliek/roost/releases),
open it, and drag `Roost.app` into `/Applications`.

### First launch on macOS

Roost is currently **ad-hoc-signed but not yet notarized** (pending an Apple
Developer account — see [#83](https://github.com/charliek/roost/issues/83)), so
macOS Gatekeeper blocks the first launch. You only have to clear this once.

**Easiest (works on every supported macOS):** after dragging Roost to
`/Applications`, run this once in Terminal, then open Roost normally:

```bash
xattr -dr com.apple.quarantine /Applications/Roost.app
```

**Or via the GUI (macOS 15+):** double-click Roost, dismiss the "Apple could not
verify…" warning, then open **System Settings → Privacy & Security**, scroll to
the message about Roost, and click **Open Anyway** (it may ask for Touch ID or
your password). Note that the older right-click → Open shortcut no longer
bypasses Gatekeeper on macOS 15+ (Roost's minimum), so use one of the two paths
above.

Once a notarized build ships, this step goes away — Roost will open with a
normal double-click.

The rest of this page covers building from source.

## Building from source

Roost ships a native UI on each platform. Each UI embeds the
workspace + PTY supervisor in-process and serves a JSON IPC
socket for external tooling (`roostctl`, Claude Code hooks):

| Platform | UI | How it builds |
|---|---|---|
| macOS | Swift + AppKit (`Roost.app`) | SwiftPM via `mac/scripts/bundle.sh` |
| Linux (packaged `.deb`) | Rust + iced (`roost-iced`) | `linux/scripts/build-deb.sh`, built with `--features roost-iced/linux-package` |
| Linux (build from source) | Rust + gtk4-rs (`roost-linux`) | `cargo build -p roost-linux` |

The `roost-iced` and `roost-linux` UIs, the `roostctl` CLI, the JSON
IPC crate, and the `libghostty-vt` FFI all live in one Cargo
workspace under `crates/`. The Swift UI is its own SwiftPM package
under `mac/` and links the same vendored `libghostty-vt` static
archive.

The Linux `.deb` ships the iced UI as `/usr/bin/roost` — see
[Paths & Environment](../reference/paths.md) for how the packaged
binary resolves the same profile (socket, `state.json`, log dir) the
GTK package used to own. `roost-linux` (gtk4-rs) still lives in this
repo and is still built and tested in CI; it's the UI to build from
source if you're contributing to the GTK side or want it running
alongside iced during development.

`mac/scripts/bundle.sh` embeds `target/<config>/roostctl` under
`Roost.app/Contents/Resources/bin/roostctl` so a packaged .app is
self-contained for `claude install`.

## Prerequisites

| Tool | Purpose | Pinned version |
|---|---|---|
| Rust | CLI + Linux UIs | 1.97.1 (via `mise`) |
| Zig | Builds `libghostty-vt` from the vendored Ghostty source | 0.15.x (via `mise`) |
| Xcode Command Line Tools | Builds the Mac UI | macOS only |
| GTK4 + libadwaita dev packages | Linker dependencies for `roost-linux` (source builds only — not needed for the packaged `.deb`, which ships iced) | Linux only |
| `mise` | Manages the pinned Rust + Zig versions | any |

## macOS

Install system packages:

```bash
brew install mise
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

Build the `roostctl` CLI:

```bash
~/.cargo/bin/cargo build --release -p roost-cli
```

Bundle the Mac `.app` (this builds and embeds `roostctl` for you):

```bash
./mac/scripts/bundle.sh release
open mac/build/Roost.app
```

### macOS 26 (Tahoe) `libghostty-vt` shim

`third_party/ghostty/build.sh` ships an `arm64-macos` SDK shim. When it detects macOS 26+ on Apple Silicon with an `arm64e`-only system SDK, it redirects Zig's SDK lookup to a sibling `MacOSX1[45].sdk` for the duration of the `zig build` call. Xcode Command Line Tools usually keeps one prior major SDK installed; reinstall (`xcode-select --install`) if you hit the `no sibling MacOSX1[45].sdk` error.

## Linux (Ubuntu / Debian)

This section builds `roost-linux`, the gtk4-rs UI, from source — useful
for contributing to the GTK side or running it alongside iced during
development. If you just want the packaged `.deb` (iced UI), see
[Shipping builds](#shipping-builds) above; building that package
yourself doesn't need GTK — see
[Building the `.deb` package](#building-the-deb-package) below.

System packages:

```bash
sudo apt update
sudo apt install -y \
  build-essential git curl pkgconf \
  libgtk-4-dev libadwaita-1-dev
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

Build the Linux UI and CLI. `roost-linux` requires the GTK4 + libadwaita system packages above; `cargo build` without `-p` skips it so contributors who only iterate on the CLI or IPC crates don't need GTK installed.

```bash
~/.cargo/bin/cargo build --release \
  -p roost-cli -p roost-linux
```

Run the Linux UI:

```bash
~/.cargo/bin/cargo run --release -p roost-linux
```

### Building the `.deb` package

The packaged `.deb` ships the iced UI (`roost-iced`, built with
`--features roost-iced/linux-package`) as `/usr/bin/roost`, plus
`roostctl` — no GTK packages required. Instead:

```bash
sudo apt-get install -y libclang-dev pkg-config
```

Then, with `mise install` and `./third_party/ghostty/build.sh` already
done (above) and [`nfpm`](https://nfpm.goreleaser.com) on `PATH`:

```bash
./linux/scripts/build-deb.sh 0.0.1-dev
```

See [`linux/README.md`](https://github.com/charliek/roost/blob/main/linux/README.md) and
[`packaging/nfpm.yaml`](https://github.com/charliek/roost/blob/main/packaging/nfpm.yaml) for what the
package contains.

## CLI on PATH

Install `roostctl` so it's reachable from any shell (Claude Code hooks call it without a full path):

```bash
sudo install -m 755 target/release/roostctl /usr/local/bin/roostctl
```

## Verifying the install

With the UI running:

```bash
~/.cargo/bin/cargo run --release -p roostctl -- identify
```

Prints a JSON object with the running UI's socket path, PID, and active project / tab IDs. If you see a connection error, the UI isn't running or the socket path is wrong — see [Paths & Environment](../reference/paths.md).

## Updating

When the pinned Ghostty SHA changes, re-build `libghostty-vt`:

```bash
./third_party/ghostty/build.sh --force
~/.cargo/bin/cargo build --release
```

`--force` discards the cached Ghostty source tree and re-clones at the new SHA. After it finishes, the Mac UI's next `bundle.sh` run picks up the new archive automatically.
