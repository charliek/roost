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
and install it with apt, substituting the version and architecture you
downloaded — e.g. `sudo apt install ./roost_0.0.18_amd64.deb`.

macOS — download `Roost-<version>.dmg` from the
[latest GitHub release](https://github.com/charliek/roost/releases),
open it, and drag `Roost.app` into `/Applications`. Release DMGs
(v0.0.18 onward) are Developer-ID signed and notarized by Apple, so
Roost opens with a normal double-click — no Gatekeeper detour.

### `Roost-Iced.app` (experimental)

The same release also publishes `Roost-Iced-<version>.dmg`. That is the
**iced** UI — the Rust codebase Linux ships — built for macOS, packaged as
`Roost-Iced.app`. It is experimental: `Roost.app` (Swift + AppKit) remains
the supported Mac product.

It is deliberately built to sit **beside** the Swift app rather than
replace it:

- Its own bundle id (`ai.stridelabs.Roost.iced`) and its own app name, so
  Dock, Cmd-Tab and Finder treat it as a separate app.
- Its own state: the `Iced` bundle profile (`~/Library/Application Support/Roost-iced/`,
  `~/Library/Caches/Roost-iced/roost.sock`, `~/Library/Logs/Roost-iced/`) —
  see [Paths & Environment](../reference/paths.md). Your Swift-app projects
  and tabs are untouched, and both can run at once.
- Its own Sparkle feed (`appcast-iced.xml`), so updating one never offers
  the other.

Install it the same way: open the DMG, drag `Roost-Iced.app` to
`/Applications`. It is signed and notarized on the same terms as
`Roost.app`. Try it if you want the newer renderer (iced + wgpu) or want to
report parity gaps against the Swift app; stay on `Roost.app` otherwise.

Note that `roostctl` auto-detects a single running UI, but with both apps
running it will ask you to choose — pass `--target mac` or `--target iced`.

### Updating

- **macOS.** Both apps check for updates through
  [Sparkle](https://sparkle-project.org), and checks are **user-invoked**:
  choose **Check for Updates…** from the app menu. Neither app polls in the
  background (`SUEnableAutomaticChecks` is `false` in both bundles), so you
  will never get an unprompted update panel. The feeds are served from this
  documentation site — `appcast.xml` for `Roost.app`, `appcast-iced.xml`
  for `Roost-Iced.app` — and each app only ever sees its own.
- **Linux.** Updates come from apt: `sudo apt update && sudo apt upgrade`
  (or just `sudo apt install --only-upgrade roost`). The package upgrades
  in place — same socket, `state.json`, and log paths as the version it
  replaces.

The rest of this page covers building from source.

## Building from source

Roost ships a native UI on each platform. Each UI embeds the
workspace + PTY supervisor in-process and serves a JSON IPC
socket for external tooling (`roostctl`, Claude Code hooks):

| Platform | UI | How it builds |
|---|---|---|
| macOS | Swift + AppKit (`Roost.app`) | SwiftPM via `mac/scripts/bundle.sh` |
| macOS (experimental) | Rust + iced (`Roost-Iced.app`) | `mac/scripts/bundle-iced.sh` |
| Linux | Rust + iced (`roost`) | `cargo build -p roost-iced -p roost-cli`; the `.deb` adds `--features roost-iced/linux-package` |

The `roost-iced` UI, the `roostctl` CLI, the JSON IPC crate, and the
`libghostty-vt` FFI all live in one Cargo workspace under `crates/`. The
Swift UI is its own SwiftPM package under `mac/` and links the same
vendored `libghostty-vt` static archive.

The Linux `.deb` ships the iced UI as `/usr/bin/roost`. The
`linux-package` Cargo feature is the only difference between a dev build
and the packaged one: it flips the compiled-in bundle profile to `Linux`,
so the installed binary resolves the production socket / `state.json` /
log paths every previous Linux release used. See
[Paths & Environment](../reference/paths.md).

`mac/scripts/bundle.sh` embeds `target/<config>/roostctl` under
`Roost.app/Contents/Resources/bin/roostctl` so a packaged .app is
self-contained: that is the binary each tab's `$ROOST_AGENT_HOOK` points
at, and the one the shell integration and agent hooks run.

## Prerequisites

| Tool | Purpose | Pinned version |
|---|---|---|
| Rust | CLI + the iced UI | 1.97.1 (via `mise`) |
| Zig | Builds `libghostty-vt` from the vendored Ghostty source | 0.16.x (via `mise`) |
| Xcode Command Line Tools | Builds the Mac UI | macOS only |
| `libclang-dev` + `pkg-config` | Build-time deps of the Rust UI's FFI bindings | Linux only |
| `mise` | Manages the pinned Rust + Zig versions | any |

## macOS

Install system packages:

```bash
brew install mise
```

Recommended: JetBrains Mono. Roost defaults to the system monospace; this
is the family the docs' examples set via `font-family` (see
[Fonts](../reference/fonts.md)).

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

Optionally, bundle the experimental iced app the same way. It resolves the
isolated `Iced` bundle profile, so it will not touch `Roost.app`'s state.
A locally bundled `Roost-Iced.app` deliberately carries **no** Sparkle
feed URL — only the release pipeline stamps one in — so **Check for
Updates…** does nothing in a local build:

```bash
./mac/scripts/bundle-iced.sh release
open mac/build/Roost-Iced.app
```

### macOS 26 (Tahoe) `libghostty-vt` shim

`third_party/ghostty/build.sh` ships an `arm64-macos` SDK shim. When it detects macOS 26+ on Apple Silicon with an `arm64e`-only system SDK, it redirects Zig's SDK lookup to a sibling `MacOSX1[45].sdk` for the duration of the `zig build` call. Xcode Command Line Tools usually keeps one prior major SDK installed; reinstall (`xcode-select --install`) if you hit the `no sibling MacOSX1[45].sdk` error.

## Linux (Ubuntu / Debian)

This section builds the iced UI from source. If you just want to *use*
Roost, install the `.deb` — see [Shipping builds](#shipping-builds) above.

System packages:

```bash
sudo apt update
sudo apt install -y \
  build-essential git curl pkg-config libclang-dev
```

That is the whole build-time set. At **run** time the UI needs winit's
keyboard/window client libraries and a wgpu backend; on a normal desktop
they are already installed, and the `.deb` declares them explicitly
(`libxkbcommon0`, `libxkbcommon-x11-0`, `libwayland-client0`, `libx11-6`,
`libx11-xcb1`, `libxcb1`, `libxcursor1`, `libxi6`, plus `libegl1` +
`libwayland-egl1` for the GLES fallback). Vulkan is *recommended*, not
required — with no usable Vulkan ICD, wgpu falls back to its compiled-in
software renderer and the app still opens a window. Both Wayland and X11
sessions are supported.

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

Build the Linux UI and CLI:

```bash
~/.cargo/bin/cargo build --release \
  -p roost-iced -p roost-cli
```

Run it:

```bash
~/.cargo/bin/cargo run --release -p roost-iced
```

A build without `--features roost-iced/linux-package` resolves the
isolated `Iced` bundle profile, so it runs beside an installed `roost`
without touching its state — see
[Paths & Environment](../reference/paths.md).

### Building the `.deb` package

The packaged `.deb` is the same binary built with
`--features roost-iced/linux-package`, installed as `/usr/bin/roost`,
plus `roostctl`. With `mise install` and `./third_party/ghostty/build.sh`
already done (above) and [`nfpm`](https://nfpm.goreleaser.com) on `PATH`:

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
~/.cargo/bin/cargo run --release -p roost-cli -- identify
```

Prints `key=value` lines with the running UI's socket path, PID, and active project / tab IDs. If you see a connection error, the UI isn't running or the socket path is wrong — see [Paths & Environment](../reference/paths.md).

## Updating

When the pinned Ghostty SHA changes, re-build `libghostty-vt`:

```bash
./third_party/ghostty/build.sh --force
~/.cargo/bin/cargo build --release
```

`--force` discards the cached Ghostty source tree and re-clones at the new SHA. After it finishes, the Mac UI's next `bundle.sh` run picks up the new archive automatically.
