#!/usr/bin/env bash
# Build roost packages INSIDE a shed, keeping every artifact shed-local
# so the VirtioFS-mounted repo's macOS build outputs are never clobbered.
#
# The repo is mounted (different arch), and three build outputs use hardcoded
# in-tree paths: cargo `target/`, ghostty `third_party/ghostty/{src,out}` (both
# build.sh's OUT_DIR/GHOSTTY_SRC and roost-vt/build.rs read the fixed path). We
# redirect target/ via CARGO_TARGET_DIR and shadow the two ghostty dirs with
# bind-mounts onto shed-local storage. Re-runnable; bind-mounts reset each boot.
#
# ROOST_SHED_PACKAGES selects which cargo packages to build (space-separated
# crate names, e.g. "roost-iced roost-cli roost-session"). Default matches
# shed-test.sh's long-standing behavior: `roost-iced roost-cli` only —
# tools/session/dev-session.sh is what sets it to `roost-session` for the
# host-sessions dev loop.
set -euo pipefail
log() { printf '[build-in-shed] %s\n' "$*"; }
die() { printf '[build-in-shed] error: %s\n' "$*" >&2; exit 1; }

REPO="${ROOST_REPO:-$HOME/roost}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/rt}"
GH_SRC="$HOME/ghostty-src"
GH_OUT="$HOME/ghostty-out"
PACKAGES="${ROOST_SHED_PACKAGES:-roost-iced roost-cli}"

mkdir -p "$GH_SRC" "$GH_OUT" "$CARGO_TARGET_DIR"
for pair in "$GH_SRC:$REPO/third_party/ghostty/src" "$GH_OUT:$REPO/third_party/ghostty/out"; do
  src="${pair%%:*}"; dst="${pair##*:}"
  # A dangling symlink here means the Mac side symlinked the dir at a target
  # outside the VirtioFS mount; `mkdir -p` then fails with a bare "File exists".
  if [ -L "$dst" ] && [ ! -e "$dst" ]; then
    die "$dst is a symlink whose target isn't visible here — replace it with a real (or empty) directory before building in a shed"
  fi
  mkdir -p "$dst"
  mountpoint -q "$dst" || { log "bind $src -> $dst (keep the Mac's ghostty untouched)"; sudo mount --bind "$src" "$dst"; }
done

cd "$REPO"
log "ghostty (zig) build — cached after first run"
./third_party/ghostty/build.sh

read -ra pkgs <<< "$PACKAGES"
cargo_args=()
for p in "${pkgs[@]}"; do cargo_args+=(-p "$p"); done
log "cargo build: ${PACKAGES} (target -> $CARGO_TARGET_DIR)"
cargo build "${cargo_args[@]}"
log "done — binaries under $CARGO_TARGET_DIR/debug/"
