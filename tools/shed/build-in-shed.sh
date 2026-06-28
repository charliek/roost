#!/usr/bin/env bash
# Build roost-linux + roostctl INSIDE a shed, keeping every artifact shed-local
# so the VirtioFS-mounted repo's macOS build outputs are never clobbered.
#
# The repo is mounted (different arch), and three build outputs use hardcoded
# in-tree paths: cargo `target/`, ghostty `third_party/ghostty/{src,out}` (both
# build.sh's OUT_DIR/GHOSTTY_SRC and roost-vt/build.rs read the fixed path). We
# redirect target/ via CARGO_TARGET_DIR and shadow the two ghostty dirs with
# bind-mounts onto shed-local storage. Re-runnable; bind-mounts reset each boot.
set -euo pipefail
log() { printf '[build-in-shed] %s\n' "$*"; }

REPO="${ROOST_REPO:-$HOME/roost}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/rt}"
GH_SRC="$HOME/ghostty-src"
GH_OUT="$HOME/ghostty-out"

mkdir -p "$GH_SRC" "$GH_OUT" "$CARGO_TARGET_DIR"
for pair in "$GH_SRC:$REPO/third_party/ghostty/src" "$GH_OUT:$REPO/third_party/ghostty/out"; do
  src="${pair%%:*}"; dst="${pair##*:}"
  mkdir -p "$dst"
  mountpoint -q "$dst" || { log "bind $src -> $dst (keep the Mac's ghostty untouched)"; sudo mount --bind "$src" "$dst"; }
done

cd "$REPO"
log "ghostty (zig) build — cached after first run"
./third_party/ghostty/build.sh
log "cargo build roost-linux + roostctl (target -> $CARGO_TARGET_DIR)"
cargo build -p roost-linux -p roost-cli
log "done: $CARGO_TARGET_DIR/debug/{roost,roostctl}"
