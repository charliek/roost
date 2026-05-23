#!/usr/bin/env bash
# Build the Roost Linux .deb.
#
# Builds the gtk4-rs UI (`roost`) + the `roostctl` CLI in release config, stages
# them with the packaging assets, and runs nfpm to emit
#   out/roost_<version>_<arch>.deb
#
# This is the developer-facing entry point — the release CI calls it too. Run it
# on the target architecture (no cross-compile): an amd64 deb is built on amd64,
# arm64 on arm64.
#
# Prerequisites (Ubuntu/Debian):
#   sudo apt-get install -y libgtk-4-dev libadwaita-1-dev pkg-config
#   mise install            # rust (rust-toolchain.toml) + zig 0.15.x
#   nfpm on PATH            # https://nfpm.goreleaser.com
#
# Usage:
#   ./linux/scripts/build-deb.sh 0.2.0
#   ./linux/scripts/build-deb.sh 0.0.1-dev      # local dev build
set -euo pipefail

VERSION="${1:-}"
if [ -z "${VERSION}" ]; then
  echo "usage: $0 <version>   (e.g. 0.2.0 or 0.0.1-dev)" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
cd "${REPO_ROOT}"

# Map the host arch to a Debian arch name. Prefer dpkg when present (the real
# target); fall back to uname for non-Debian dev hosts (e.g. a Mac doing a
# layout smoke test — the resulting deb won't be functional there).
if command -v dpkg >/dev/null 2>&1; then
  ROOST_ARCH="$(dpkg --print-architecture)"
else
  case "$(uname -m)" in
    x86_64|amd64)        ROOST_ARCH="amd64" ;;
    aarch64|arm64)       ROOST_ARCH="arm64" ;;
    *) echo "error: unsupported arch $(uname -m)" >&2; exit 1 ;;
  esac
fi
export ROOST_ARCH
export ROOST_VERSION="${VERSION}"

echo "==> Building libghostty-vt (cached)"
"${REPO_ROOT}/third_party/ghostty/build.sh"

echo "==> cargo build --release (roost + roostctl)"
cargo build --release -p roost-linux -p roost-cli

CARGO_TARGET="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
ROOST_BIN="${CARGO_TARGET}/release/roost"
ROOSTCTL_BIN="${CARGO_TARGET}/release/roostctl"
for b in "${ROOST_BIN}" "${ROOSTCTL_BIN}"; do
  if [ ! -x "${b}" ]; then
    echo "error: expected binary not found: ${b}" >&2
    exit 1
  fi
done

echo "==> Staging dist/"
rm -rf "${REPO_ROOT}/dist"
mkdir -p "${REPO_ROOT}/dist"
install -m 0755 "${ROOST_BIN}"    "${REPO_ROOT}/dist/roost"
install -m 0755 "${ROOSTCTL_BIN}" "${REPO_ROOT}/dist/roostctl"

echo "==> nfpm pkg (version=${ROOST_VERSION}, arch=${ROOST_ARCH})"
mkdir -p "${REPO_ROOT}/out"
nfpm pkg --packager deb --config "${REPO_ROOT}/packaging/nfpm.yaml" --target "${REPO_ROOT}/out/"

echo "==> Built:"
ls -1 "${REPO_ROOT}"/out/*.deb
