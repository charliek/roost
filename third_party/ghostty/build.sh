#!/usr/bin/env bash
# Vendored Ghostty / libghostty-vt build for the new architecture.
#
# Mirrors the script at build/build.sh but writes into third_party/ghostty/
# and only handles libghostty-vt — the Rust workspace under crates/ links the
# resulting static archive directly via crates/roost-vt's build.rs (bindgen +
# rustc-link-search), and the Mac Xcode project under mac/ references it via
# OTHER_LDFLAGS. There is no second "build the application" stage here.
#
# Pinned SHA must move in lockstep with build/build.sh until the Phase 9
# cutover described in docs/development/vision.md, after which the legacy
# build/ directory is deleted and only this script survives.
#
# Usage:
#   ./third_party/ghostty/build.sh           # build (idempotent on cache hit)
#   ./third_party/ghostty/build.sh --force   # discard cache, rebuild from scratch

set -euo pipefail

# --- Pinned versions ---------------------------------------------------------
# Ghostty SHA — KEEP IN SYNC with build/build.sh until Phase 9 cutover.
# Bump deliberately, in its own PR, with a rebuild test on Linux + Mac CI.
GHOSTTY_SHA="c74f6d56d1feef473033057bc0ff7e3f00cf6421"  # 2026-04-25, builds lib-vt cleanly with zig 0.15.1
GHOSTTY_REPO="https://github.com/ghostty-org/ghostty.git"

# --- Paths -------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GHOSTTY_SRC="${SCRIPT_DIR}/src"
OUT_DIR="${SCRIPT_DIR}/out"

force=0
case "${1:-}" in
  --force) force=1 ;;
  "")      ;;
  *)       echo "usage: $0 [--force]"; exit 1 ;;
esac

if [ "${force}" -eq 1 ]; then
  echo "==> --force: removing ${GHOSTTY_SRC} and ${OUT_DIR}"
  rm -rf "${GHOSTTY_SRC}" "${OUT_DIR}"
fi

command -v zig >/dev/null 2>&1 || {
  echo "error: zig not found on PATH. Run \`mise install\` from the repo root." >&2
  exit 1
}

# Ghostty pins to Zig 0.15.x; both 0.14 and 0.16 produce surprising errors
# downstream. Validate up front so a wrong toolchain is obvious.
zig_version="$(zig version 2>/dev/null || true)"
if ! printf '%s\n' "${zig_version}" | grep -E '^0\.15(\.|$)' >/dev/null; then
  echo "error: zig version ${zig_version:-<unknown>} is not 0.15.x." >&2
  echo "       Install the right toolchain via \`mise install\` from the repo root." >&2
  exit 1
fi

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

missing_header=0
missing_archive=0
[ -f "${OUT_DIR}/include/ghostty/vt.h" ] || missing_header=1
[ -f "${OUT_DIR}/lib/libghostty-vt.a" ] || missing_archive=1
if [ "${missing_header}" -eq 1 ] || [ "${missing_archive}" -eq 1 ]; then
  echo "error: expected libghostty-vt artifacts not found under ${OUT_DIR} after zig build." >&2
  echo "       missing header (vt.h):                  ${missing_header}" >&2
  echo "       missing static archive (libghostty-vt.a): ${missing_archive}" >&2
  echo "       Inspect ${OUT_DIR} and ${GHOSTTY_SRC}/zig-out/." >&2
  exit 1
fi

echo "==> libghostty-vt built: ${OUT_DIR}"
echo "    header: ${OUT_DIR}/include/ghostty/vt.h"
echo "    static: ${OUT_DIR}/lib/libghostty-vt.a"
