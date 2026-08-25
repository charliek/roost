#!/usr/bin/env bash
# One-time install hook: the system + toolchain deps to BUILD and TEST roost-iced
# in a shed. Does NOT build roost itself (build-on-first-use — see
# tools/shed/shed-test.sh), but does install everything the build + the three
# test tiers (Xvfb/X11, weston/Wayland, cage+uinput drag) need.
set -euo pipefail
log() { echo "[provision $(date +%H:%M:%S)] $*"; }

# GTK4 + libadwaita are NOT a roost-iced build dependency (the gtk4-rs UI,
# crates/roost-linux, was retired — plan 031). They stay because
# tools/input/linux/iced_native_file_drop_check.py launches a small GTK app
# as its XDND drag *source* to exercise roost-iced's native file-drop
# target; roost-iced itself is GTK-free, verified independently by the
# Cargo boundary gate in `make check-iced`.
log "apt: GTK4 + libadwaita (XDND drag-source test dependency only), Wayland test compositors, X11 input, python test deps"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y -qq \
  libgtk-4-dev libadwaita-1-dev pkg-config libclang-dev clang \
  weston cage seatd \
  xvfb xdotool dbus-x11 mesa-vulkan-drivers libxkbcommon-x11-0 \
  python3-pytest wl-clipboard zsh
log "apt done"

# rust + zig are pinned in rust-toolchain.toml + mise.toml; mise ships on the
# shed base image (on the login PATH via /etc/profile.d). Trust the repo config
# and install the pinned toolchains so `cargo` / `zig` are ready for first build.
if command -v mise >/dev/null 2>&1; then
  log "mise: trust repo + install pinned rust/zig toolchains"
  mise trust -a >/dev/null 2>&1 || true
  mise install >/dev/null 2>&1 || log "WARN: mise install had issues (build may need it run again)"
else
  log "WARN: mise not found on PATH — install rust/zig manually before building"
fi
log "install hook complete"
