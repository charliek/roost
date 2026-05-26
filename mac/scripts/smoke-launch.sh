#!/usr/bin/env bash
# Clean-install launch smoke for Roost.app.
#
# Regression guard for the v0.0.2 crash. The themes resource bundle
# (`Roost_Roost.bundle`) was shipped under `Contents/Resources`, but the
# code read it through SwiftPM's generated `Bundle.module`, whose only
# search paths are `Bundle.main.bundleURL/Roost_Roost.bundle` (the .app
# ROOT — which can't hold nested bundles without breaking codesigning)
# and a build-machine path baked in at compile time. So the app
# `fatalError`ed on every clean install — yet dev + CI passed, because
# both ran on the build host where that compile-time `.build/.../
# Roost_Roost.bundle` was present and masked the bug.
#
# This script reproduces a user's machine by hiding the build-tree
# bundle, then asserts the packaged .app still launches and answers
# `identify`. Run it after `scripts/bundle.sh`. The build tree is
# restored on exit (including on failure).
set -euo pipefail

MAC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="${MAC_DIR}/build/Roost.app"
BIN="${APP}/Contents/MacOS/Roost"
ROOSTCTL="${APP}/Contents/Resources/bin/roostctl"
BUILD="${MAC_DIR}/.build"
APP_PID=""

[ -x "${BIN}" ] || { echo "error: ${BIN} not found — run scripts/bundle.sh first" >&2; exit 1; }

restore() {
  # Un-hide any bundles we moved, and stop the app we launched.
  if [ -d "${BUILD}" ]; then
    find "${BUILD}" -maxdepth 4 -name 'Roost_Roost.bundle.smokehidden' -type d 2>/dev/null \
      | while read -r d; do mv "${d}" "${d%.smokehidden}"; done
  fi
  [ -n "${APP_PID}" ] && kill "${APP_PID}" 2>/dev/null || true
}
trap restore EXIT

# Hide the accessor's compile-time fallback so only the .app's own
# resources can satisfy theme loading — the clean-install condition.
if [ -d "${BUILD}" ]; then
  find "${BUILD}" -maxdepth 4 -name 'Roost_Roost.bundle' -type d 2>/dev/null \
    | while read -r d; do mv "${d}" "${d}.smokehidden"; done
fi

osascript -e 'tell application "Roost" to quit' 2>/dev/null || true
pkill -x Roost 2>/dev/null || true
sleep 1

# Launch the packaged binary directly; a resource-resolution
# `fatalError` lands in this log instead of a crash reporter.
ERRLOG="$(mktemp)"
"${BIN}" >"${ERRLOG}" 2>&1 &
APP_PID=$!

ok=0
for _ in $(seq 1 30); do
  kill -0 "${APP_PID}" 2>/dev/null || break   # process died — launch failed
  if "${ROOSTCTL}" identify >/dev/null 2>&1; then ok=1; break; fi
  sleep 0.5
done

if [ "${ok}" = 1 ]; then
  echo "✓ clean-install launch OK: app started and answered identify (build-tree bundle hidden)"
  exit 0
fi

echo "✗ clean-install launch FAILED: the app did not answer 'identify' with the" >&2
echo "  build-tree resource bundle hidden — the v0.0.2-class regression where" >&2
echo "  resources only resolve on the build machine. Captured app output:" >&2
echo "--------------------------------------------------------------------" >&2
cat "${ERRLOG}" >&2
exit 1
