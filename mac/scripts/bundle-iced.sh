#!/usr/bin/env bash
# Roost-Iced.app bundling — M6 6a (plan 027).
#
# Wraps the `roost-iced` cargo binary into a proper macOS .app bundle
# so it can be Finder-launched / Dock-pinned / referenced by its own
# bundle identifier, side by side with the Swift Roost.app, so M6 can
# evaluate the iced UI with real macOS bundle identity: Sparkle (6c),
# UNUserNotificationCenter (6e), and TCC testing all require a
# bundled, signed .app.
#
# What this script does:
#   1. Builds `roost-iced` in the requested configuration
#      (default: release) via `cargo build -p roost-iced`.
#   2. Assembles `mac/build/Roost-Iced.app` with the standard macOS
#      bundle layout — Contents/MacOS/Roost-Iced, Contents/Info.plist,
#      Contents/Resources/.
#   3. Substitutes @VERSION@ in
#      `mac/Resources/Info-iced.plist.template` with the workspace
#      version (or $ROOST_VERSION).
#   4. Installs the app icon, reusing the same art as Roost.app
#      (shared art is a recorded decision — plan 027 § W2; a distinct
#      Roost-Iced icon is future work).
#   5. Embeds `roostctl` under Contents/Resources/bin/ (same as
#      bundle.sh).
#   6. Code-signs (ad-hoc by default, Developer ID when
#      ROOST_DEVELOPER_ID_IDENTITY is set) — no framework signing
#      stage: this bundle embeds no frameworks (no Sparkle).
#
# What this script deliberately does NOT do (unlike bundle.sh):
#   * No SwiftPM build, no SwiftPM resource-bundle copy — the iced
#     chrome fonts are `include_bytes!`'d and the themes ship
#     compiled into `roost-ui-model`; nothing is loaded from a
#     resource bundle at runtime.
#   * No Sparkle.framework embed/sign — no auto-update parity between
#     the two bundles (6c decision, not made here); see the header
#     comment in Info-iced.plist.template for why the Sparkle plist
#     keys are absent too.
#   * Code-sign with a Developer ID certificate (ad-hoc until #83,
#     same as bundle.sh).
#   * Notarize via `notarytool`.
#   * Build a DMG (out of scope — plan 027 scope brief: local build
#     only, no release.yml wiring).
#
# The toolkit-agnostic stages (version derivation, icon pipeline,
# roostctl embed, signing machinery, the libghostty-vt precondition)
# live in bundle-lib.sh, shared with bundle.sh.
#
# Note (plan 027 § 4, roadmap 6a decision, not fixed here): with both
# Roost.app and Roost-Iced.app installed, `claude install`'s
# self_exe-derived hook path points at whichever bundle's roostctl
# ran it last — running it from Roost-Iced.app points Claude hooks at
# this bundle's embedded roostctl, and vice versa for Roost.app. Not
# a 6a blocker; noted for later slices.
#
# Usage:
#   ./mac/scripts/bundle-iced.sh                 # release build
#   ./mac/scripts/bundle-iced.sh debug           # debug build
#   ROOST_VERSION=0.2.0 ./mac/scripts/bundle-iced.sh
#
#   open mac/build/Roost-Iced.app                # launch the bundle

set -euo pipefail

CONFIG="${1:-release}"
case "${CONFIG}" in
  release|debug) ;;
  *)
    echo "error: configuration must be 'release' or 'debug', got '${CONFIG}'" >&2
    exit 1
    ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAC_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${MAC_DIR}/.." && pwd)"

# shellcheck source=mac/scripts/bundle-lib.sh
source "${SCRIPT_DIR}/bundle-lib.sh"

# Default the marketing version to the workspace's single source of
# truth — see roost_workspace_version in bundle-lib.sh — so a local
# `bundle-iced.sh debug` reports the same version the Swift app, the
# GTK UI, the .deb, and `roostctl identify` do.
VERSION="${ROOST_VERSION:-$(roost_workspace_version "${REPO_ROOT}")}"
APP_NAME="Roost-Iced"
BUNDLE_ID="ai.stridelabs.Roost.iced"
TEMPLATE_PLIST="${MAC_DIR}/Resources/Info-iced.plist.template"
# Shared icon art with Roost.app (plan 027 § W2 — recorded decision;
# a distinct Roost-Iced icon is future work).
ICON_SRC="${MAC_DIR}/Resources/AppIcon.icns"
ICON_COMPOSER_SRC="${MAC_DIR}/AppIcon.icon"

OUT_DIR="${MAC_DIR}/build"
APP_DIR="${OUT_DIR}/${APP_NAME}.app"

roost_check_libghostty_archive "${REPO_ROOT}"

CARGO_BIN="$(command -v cargo || true)"
if [ -z "${CARGO_BIN}" ] && [ -x "${HOME}/.cargo/bin/cargo" ]; then
  CARGO_BIN="${HOME}/.cargo/bin/cargo"
fi
if [ -z "${CARGO_BIN}" ]; then
  echo "error: cargo not found on PATH or at ~/.cargo/bin/cargo" >&2
  exit 1
fi

CARGO_PROFILE_FLAG="--release"
CARGO_PROFILE_DIR="release"
if [ "${CONFIG}" = "debug" ]; then
  CARGO_PROFILE_FLAG=""
  CARGO_PROFILE_DIR="debug"
fi

echo "==> Building roost-iced (cargo build -p roost-iced --${CARGO_PROFILE_DIR})"
(
  cd "${REPO_ROOT}"
  # shellcheck disable=SC2086  # CARGO_PROFILE_FLAG must word-split (empty => no flag)
  "${CARGO_BIN}" build -p roost-iced ${CARGO_PROFILE_FLAG}
)

# Respect CARGO_TARGET_DIR for artifact discovery, exactly like the
# roostctl embed step in bundle-lib.sh — shared caches (sccache, CI
# matrices fanning out across configs) routinely override the default
# `<repo>/target/` location.
CARGO_TARGET="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
# Cargo resolves a relative CARGO_TARGET_DIR from its own CWD (the repo
# root, where the build subshell cd's) — anchor discovery the same way so
# the two can't diverge.
case "${CARGO_TARGET}" in
  /*) ;;
  *) CARGO_TARGET="${REPO_ROOT}/${CARGO_TARGET}" ;;
esac
ICED_BUILD_BIN="${CARGO_TARGET}/${CARGO_PROFILE_DIR}/roost-iced"
if [ ! -x "${ICED_BUILD_BIN}" ]; then
  echo "error: cargo build did not produce ${ICED_BUILD_BIN}" >&2
  exit 1
fi

roost_assemble_skeleton "${APP_DIR}"

cp "${ICED_BUILD_BIN}" "${APP_DIR}/Contents/MacOS/${APP_NAME}"
chmod +x "${APP_DIR}/Contents/MacOS/${APP_NAME}"

roost_stamp_plist "${TEMPLATE_PLIST}" "${APP_DIR}" "${VERSION}"
roost_write_pkginfo "${APP_DIR}"

roost_install_app_icon "${ICON_COMPOSER_SRC}" "${ICON_SRC}" "${APP_DIR}"

# M8-parity: embed roostctl under Contents/Resources/bin/ so `claude
# install` invoked from inside Roost-Iced.app writes hook paths that
# point at the bundled binary, not a dev-machine target/ path.
roost_build_and_embed_roostctl "${REPO_ROOT}" "${APP_DIR}" "${CONFIG}"

# Signing. When ROOST_DEVELOPER_ID_IDENTITY is set (release CI, or a dev who
# holds the cert) we sign with that Developer ID + a secure `--timestamp` so the
# bundle can be notarized. Otherwise we fall back to ad-hoc (`-`) signing: fine
# for local launch, but Gatekeeper will warn and notarization is impossible.
# The inner→outer order (embedded roostctl first, then the .app) is required —
# codesign seals nested code into the outer signature. No framework-signing
# stage: unlike Roost.app, this bundle embeds no frameworks.
ENT_FILE="${MAC_DIR}/Resources/Roost-Iced.entitlements"
# The bundled roostctl helper gets the same narrower entitlements file
# bundle.sh uses: it never records audio/video or sends Apple events,
# so it must not inherit the app's capture entitlements.
ROOSTCTL_ENT_FILE="${MAC_DIR}/Resources/roostctl.entitlements"
if roost_setup_signing "${ENT_FILE}" "${ROOSTCTL_ENT_FILE}"; then
  codesign_or_die "${APP_DIR}/Contents/Resources/bin/roostctl" "${ROOSTCTL_ENT_FILE}"
  codesign_or_die "${APP_DIR}"
fi

echo "==> Bundled: ${APP_DIR}"
echo "    Bundle ID:    ${BUNDLE_ID}"
echo "    Version:      ${VERSION}"
echo "    Executable:   ${APP_DIR}/Contents/MacOS/${APP_NAME}"
echo "    Embedded CLI: ${APP_DIR}/Contents/Resources/bin/roostctl"
echo
echo "Note: with both Roost.app and Roost-Iced.app installed, 'claude"
echo "install' points Claude's hook file at whichever bundle's roostctl"
echo "ran it last (roadmap 6a decision — noted, not fixed here)."
echo
echo "Launch with: open '${APP_DIR}'"
