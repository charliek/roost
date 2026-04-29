# Installation

Roost ships as two binaries — `roost` (GUI) and `roost-cli` (companion). Both are built from the same Go module against a pinned Ghostty source tree compiled with Zig.

## Prerequisites

| Tool                 | Purpose                                            | Pinned version |
|----------------------|----------------------------------------------------|----------------|
| Go                   | Builds the Roost binaries                          | 1.24+          |
| Zig                  | Compiles libghostty-vt from the Ghostty source     | 0.15.2         |
| GTK4 + libadwaita    | UI toolkit (linked at build time and at runtime)   | 4.x / 1.x      |
| pkg-config (`pkgconf`) | Resolves GTK / glib / cairo include + lib paths  | any            |
| gobject-introspection | Required at build time by gotk4                   | 1.x            |
| `mise`               | Manages the pinned Go and Zig versions             | any            |
| `git`                | Clones Ghostty during the libghostty-vt build      | any            |

## macOS (Homebrew)

Install the system packages:

```bash
brew install gtk4 libadwaita pkgconf gobject-introspection
```

Recommended: install JetBrains Mono. It's the default font Roost looks for and renders well through Pango/Cairo on macOS:

```bash
brew install --cask font-jetbrains-mono
```

Roost falls back to Monaco if JetBrains Mono isn't installed, but the fallback path on macOS is finicky (Pango can drop to Verdana when the requested family is missing) — see [Config keys](../reference/paths.md#config-keys) to override the family or set up your own preference order.

For clickable, supersedable desktop notifications on macOS, install `terminal-notifier`:

```bash
brew install terminal-notifier
```

Without it, in-app indicators (tab icons, project rollup stripe) keep working but desktop banners are silent no-ops. Roost-branded macOS notifications need a code-signed `.app` bundle, which is separate work; until then banners show "terminal-notifier" as the source. See [Notifications](../guides/notifications.md) for details.

If you don't already have [`mise`](https://mise.jdx.dev/), install it:

```bash
brew install mise
# Add `eval "$(mise activate zsh)"` (or bash equivalent) to your shell rc.
```

Clone the repo and pull pinned tools:

```bash
git clone https://github.com/charliek/roost.git
cd roost
mise install            # provisions Go 1.24 and Zig 0.15.2
```

Build libghostty-vt and the Roost binaries:

```bash
make libghostty         # clones Ghostty at the pinned SHA, runs zig build -Demit-lib-vt
make build              # produces ./roost and ./roost-cli at the repo root
```

Run it:

```bash
./roost
```

Optionally install `roost-cli` on your `PATH` so Claude Code hooks (and any tab) can call it without a full path:

```bash
sudo install -m 755 ./roost-cli /usr/local/bin/roost-cli
```

## Linux (Ubuntu / Debian)

System packages:

```bash
sudo apt update
sudo apt install -y \
  build-essential git curl \
  libgtk-4-dev libadwaita-1-dev \
  pkgconf gobject-introspection libgirepository1.0-dev
```

Recommended: install JetBrains Mono (the default Roost looks for). On Debian/Ubuntu:

```bash
sudo apt install -y fonts-jetbrains-mono
```

`mise` install (one-time, [official instructions](https://mise.jdx.dev/getting-started.html)):

```bash
curl https://mise.run | sh
echo 'eval "$(mise activate bash)"' >> ~/.bashrc
```

Then the same Roost build steps:

```bash
git clone https://github.com/charliek/roost.git
cd roost
mise install
make libghostty
make build
./roost
```

Install the CLI on `PATH`:

```bash
sudo install -m 755 ./roost-cli /usr/local/bin/roost-cli
```

## What `make libghostty` does

It runs `./build/build.sh libghostty`, which:

1. Clones [`ghostty-org/ghostty`](https://github.com/ghostty-org/ghostty) at a pinned commit SHA into `build/ghostty-src/`.
2. Runs `zig build -Demit-lib-vt=true -Doptimize=ReleaseFast` against that checkout.
3. Installs the artifact (`libghostty-vt.a`, `libghostty-vt.dylib` / `.so`, headers under `ghostty/vt/`) into `build/out/`.

The pinned SHA is the only place to bump Ghostty's terminal engine. Don't bump it casually — the libghostty-vt API is documented as unstable.

## Verifying the install

```bash
./roost &           # GUI window opens
./roost-cli identify
```

`roost-cli identify` should print a JSON object containing the socket path and the active project / tab IDs. If you see a connection error, the GUI isn't running or the socket path is wrong — see [Paths & Environment](../reference/paths.md).

## Updating

When the pinned Ghostty SHA changes you must rebuild the library:

```bash
make clean
make libghostty
make build
```

`make clean` removes `build/out/`, `build/ghostty-src/`, and the binaries. The next `make libghostty` re-clones at the new SHA.
