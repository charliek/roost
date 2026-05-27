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
# `identify`. It launches the real GUI app, so it's a LOCAL / pre-release
# check (`make smoke-mac-launch`) — NOT a per-PR CI gate: CI GUI sessions
# are flaky (the same reason e2e-mac is non-gating, and a second launch in
# one job races the single-instance lock). Per-PR CI instead asserts the
# themes bundle ships under Contents/Resources (deterministic, in the
# required swift-mac job). Run after `scripts/bundle.sh`; the build tree is
# restored on exit (including on failure).
set -euo pipefail

MAC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="${MAC_DIR}/build/Roost.app"
ROOSTCTL="${APP}/Contents/Resources/bin/roostctl"
BUILD="${MAC_DIR}/.build"

[ -x "${APP}/Contents/MacOS/Roost" ] \
  || { echo "error: ${APP} not built — run scripts/bundle.sh first" >&2; exit 1; }

quit_app() {
  osascript -e 'tell application "Roost" to quit' 2>/dev/null || true
  pkill -x Roost 2>/dev/null || true
  # Wait for the process to actually exit so the single-instance lock +
  # socket are released before we return — don't race a follow-on launch.
  for _ in $(seq 1 20); do pgrep -x Roost >/dev/null 2>&1 || break; sleep 0.25; done
}

restore() {
  quit_app
  if [ -d "${BUILD}" ]; then
    find "${BUILD}" -maxdepth 4 -name 'Roost_Roost.bundle.smokehidden' -type d -print0 2>/dev/null \
      | while IFS= read -r -d '' d; do mv "${d}" "${d%.smokehidden}"; done
  fi
}
trap restore EXIT

# Hide the accessor's compile-time fallback so only the .app's own
# resources can satisfy theme loading — the clean-install condition.
if [ -d "${BUILD}" ]; then
  find "${BUILD}" -maxdepth 4 -name 'Roost_Roost.bundle' -type d -print0 2>/dev/null \
    | while IFS= read -r -d '' d; do mv "${d}" "${d}.smokehidden"; done
fi

quit_app
sleep 1

# Launch the way Finder / the e2e harness do (LaunchServices via `open`),
# NOT by exec'ing the binary directly — a normally-launched .app gets the
# correct `Bundle.main` (and thus `Contents/Resources`), which a bare
# `exec` of the inner binary does not on every macOS version.
open "${APP}"

ok=0
for _ in $(seq 1 40); do
  if "${ROOSTCTL}" identify >/dev/null 2>&1; then ok=1; break; fi
  sleep 0.5
done

if [ "${ok}" = 1 ]; then
  echo "✓ clean-install launch OK: app started and answered identify (build-tree bundle hidden)"
  exit 0
fi

echo "✗ clean-install launch FAILED: the app did not answer 'identify' within 20s with" >&2
echo "  the build-tree resource bundle hidden — the v0.0.2-class regression where" >&2
echo "  resources only resolve on the build machine." >&2
echo "--- app log tail ---" >&2
tail -12 "${HOME}/Library/Logs/Roost/roost.log" 2>/dev/null >&2 || echo "  (no app log)" >&2
crash="$(ls -t "${HOME}/Library/Logs/DiagnosticReports/"Roost-*.ips 2>/dev/null | head -1 || true)"
if [ -n "${crash}" ]; then
  echo "--- latest crash report (${crash##*/}) ---" >&2
  grep -iE "termination|exception|fatal|resource_bundle|Roost_Roost" "${crash}" 2>/dev/null | head -8 >&2 || true
fi
exit 1
