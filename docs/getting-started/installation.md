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
are Developer-ID signed and notarized by Apple, so
Roost opens with a normal double-click — no Gatekeeper detour.

`Roost.app` is the Rust + iced UI, the same codebase Linux ships, with
the full agent-facing surface (`roostctl open`/`wait`/`events`, `tab prompt`,
the [Claude skill](https://github.com/charliek/roost/tree/main/skills/roost)).
Before v0.1.0 `Roost.app` was a Swift + AppKit build; that app is retired. It
keeps the same bundle id (`ai.stridelabs.Roost`) and the same state paths, so
an existing install updates in place through Sparkle and keeps its projects,
socket and logs.

### `Roost-Iced.app` users

`Roost-Iced.app` (bundle id `ai.stridelabs.Roost.iced`) was the experimental
iced build published beside the Swift app. It cannot update itself into
`Roost.app`: its **Check for Updates…** now shows a notice pointing here
instead of offering an update. Switch by hand:

1. Install `Roost.app` from the DMG above.
2. Quit both apps, then delete `Roost-Iced.app`.
3. Re-run `roostctl agent install`. Agent hooks written earlier point at
   `Roost-Iced.app`'s copy of `roostctl`, which no longer exists once the
   app is deleted.
4. Optionally copy your projects and tabs across. Roost-Iced kept them in
   its own directory, and `Roost.app` does not read it:

   ```bash
   cd ~/Library/Application\ Support
   mkdir -p Roost
   cp -n Roost-iced/state.json Roost/state.json
   ```

   `-n` keeps any `state.json` that `Roost.app` already has, from the Swift
   app or from a first launch; move that one aside first if you want
   Roost-Iced's instead. Run it while both apps are quit. Your config (`~/.config/roost/config.conf`) is shared
   and needs nothing.

Dev builds (`make bundle-iced`) are still named `Roost-Iced.app` and still
use the separate `Roost-iced` paths, so they run beside an installed
`Roost.app`; see [Paths & Environment](../reference/paths.md).

### Updating

- **macOS.** `Roost.app` checks for updates through
  [Sparkle](https://sparkle-project.org), and checks are **user-invoked**:
  choose **Check for Updates…** from the app menu. It does not poll in the
  background (`SUEnableAutomaticChecks` is `false`), so you will never get
  an unprompted update panel. The feed (`appcast.xml`) is served from this
  documentation site.
- **Linux.** Updates come from apt: `sudo apt update && sudo apt upgrade`
  (or just `sudo apt install --only-upgrade roost`). The package upgrades
  in place — same socket, `state.json`, and log paths as the version it
  replaces.

The rest of this page covers building from source.

## Building from source

Roost ships one UI, Rust + iced, on both platforms. It embeds the
workspace + PTY supervisor in-process and serves a JSON IPC
socket for external tooling (`roostctl`, Claude Code hooks):

| Platform | UI | How it builds |
|---|---|---|
| macOS | Rust + iced (`Roost.app`) | `ROOST_BUNDLE_IDENTITY=production mac/scripts/bundle-iced.sh` (release pipeline); `make bundle-iced` for the dev `Roost-Iced.app` |
| Linux | Rust + iced (`roost`) | `cargo build -p roost-iced -p roost-cli`; the `.deb` adds `--features roost-iced/linux-package` |

The `roost-iced` UI, the `roostctl` CLI, the JSON IPC crate, and the
`libghostty-vt` FFI all live in one Cargo workspace under `crates/`.

The Linux `.deb` ships the iced UI as `/usr/bin/roost`. The
`linux-package` Cargo feature is the only difference between a dev build
and the packaged one: it flips the compiled-in bundle profile to `Linux`,
so the installed binary resolves the production socket / `state.json` /
log paths every previous Linux release used. See
[Paths & Environment](../reference/paths.md).

`mac/scripts/bundle-iced.sh` embeds `target/<config>/roostctl` under
`<app>/Contents/Resources/bin/roostctl` so a packaged .app is
self-contained: that is the binary each tab's `$ROOST_AGENT_HOOK` points
at, and the one the shell integration and agent hooks run.

## Prerequisites

| Tool | Purpose | Pinned version |
|---|---|---|
| Rust | CLI + the iced UI | 1.97.1 (via `mise`) |
| Zig | Builds `libghostty-vt` from the vendored Ghostty source | 0.17.x (via `mise`) |
| Xcode Command Line Tools | Builds the Mac UI and bundle | macOS only |
| `libclang-dev` + `pkg-config` | Build-time deps of the Rust UI's FFI bindings | Linux only |
| `mise` | Manages the pinned Rust + Zig versions | any |

## macOS

Install system packages:

```bash
brew install mise
```

Recommended: JetBrains Mono. Roost defaults to its bundled JetBrains Mono; this
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

Bundle the dev Mac `.app` (this builds and embeds `roostctl` and
`roost-session` for you):

```bash
make bundle-iced
open mac/build/Roost-Iced.app
```

A dev bundle resolves the isolated `Iced` bundle profile, so it will not
touch an installed `Roost.app`'s state, and it deliberately carries **no**
Sparkle feed URL (only the release pipeline stamps one in), so **Check for
Updates…** does nothing in a local build. The shipped `Roost.app` is the same
script run with `ROOST_BUNDLE_IDENTITY=production`, which needs the feed URL
and public key (`ROOST_BUNDLE_SPARKLE_FEED_URL`,
`ROOST_BUNDLE_SPARKLE_ED_PUBLIC_KEY`) and is normally left to CI.

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

`--force` discards the cached Ghostty source tree and re-clones at the new SHA. After it finishes, the Mac UI's next `bundle-iced.sh` run picks up the new archive automatically.
