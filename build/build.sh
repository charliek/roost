#!/usr/bin/env bash
# Roost build orchestrator.
#
# Two stages:
#   1. libghostty — clone Ghostty at a pinned SHA into build/ghostty-src,
#      run `zig build lib-vt` to produce libghostty-vt + ghostty/vt.h, install
#      to build/out/.
#   2. build — cgo-link against build/out and produce the roost + roost-cli
#      binaries at the repo root.
#
# Usage:
#   ./build/build.sh libghostty   # stage 1 only
#   ./build/build.sh build        # stage 2 only (assumes stage 1 done)
#   ./build/build.sh              # both
set -euo pipefail

# --- Pinned versions ---------------------------------------------------------
# Ghostty SHA — bump deliberately, in its own PR, with a rebuild test.
GHOSTTY_SHA="c74f6d56d1feef473033057bc0ff7e3f00cf6421"  # 2026-04-25, builds lib-vt cleanly with zig 0.15.1
GHOSTTY_REPO="https://github.com/ghostty-org/ghostty.git"

# --- Paths -------------------------------------------------------------------
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_DIR="${ROOT_DIR}/build"
GHOSTTY_SRC="${BUILD_DIR}/ghostty-src"
OUT_DIR="${BUILD_DIR}/out"

stage_libghostty() {
  command -v zig >/dev/null 2>&1 || {
    echo "error: zig not found on PATH. Run \`mise install\` from the repo root."
    exit 1
  }

  if [ ! -d "${GHOSTTY_SRC}/.git" ]; then
    echo "==> Cloning Ghostty @ ${GHOSTTY_SHA}"
    git clone "${GHOSTTY_REPO}" "${GHOSTTY_SRC}"
  fi

  pushd "${GHOSTTY_SRC}" >/dev/null
  current_sha="$(git rev-parse HEAD)"
  if [ "${current_sha}" != "${GHOSTTY_SHA}" ]; then
    echo "==> Checking out ${GHOSTTY_SHA}"
    git fetch --quiet origin "${GHOSTTY_SHA}" || git fetch --quiet
    git checkout --quiet "${GHOSTTY_SHA}"
  fi

  echo "==> Building libghostty-vt"
  zig build -Demit-lib-vt=true -Doptimize=ReleaseFast --prefix "${OUT_DIR}"
  popd >/dev/null

  if [ ! -f "${OUT_DIR}/include/ghostty/vt.h" ]; then
    echo "error: ghostty/vt.h not found in ${OUT_DIR}/include after zig build."
    echo "       Inspect ${OUT_DIR} and ${GHOSTTY_SRC}/zig-out/."
    exit 1
  fi
  echo "==> libghostty-vt built: ${OUT_DIR}"
}

stage_build() {
  command -v go >/dev/null 2>&1 || {
    echo "error: go not found on PATH. Run \`mise install\` from the repo root."
    exit 1
  }

  if [ ! -f "${OUT_DIR}/include/ghostty/vt.h" ]; then
    echo "error: libghostty-vt not built yet. Run: $0 libghostty"
    exit 1
  fi

  export CGO_CFLAGS="-I${OUT_DIR}/include"
  export CGO_LDFLAGS="-L${OUT_DIR}/lib -lghostty-vt"

  echo "==> Building roost"
  cd "${ROOT_DIR}"
  go build -o ./roost ./cmd/roost
  go build -o ./roost-cli ./cmd/roost-cli
  echo "==> Done: ./roost ./roost-cli"
}

stage="${1:-all}"
case "${stage}" in
  libghostty) stage_libghostty ;;
  build)      stage_build ;;
  all|"")     stage_libghostty; stage_build ;;
  *)          echo "usage: $0 [libghostty|build|all]"; exit 1 ;;
esac
