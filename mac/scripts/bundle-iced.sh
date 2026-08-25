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
#   6. Fetches (pinned version+SHA, cached — third_party/sparkle/
#      fetch.sh) and embeds Sparkle.framework under Contents/
#      Frameworks/ — M6 6c mechanics (plan 028). The embed runs even
#      for ROOST_ALLOW_UNSIGNED=1 builds; only the signing is
#      conditional.
#   7. Code-signs (ad-hoc by default, Developer ID when
#      ROOST_DEVELOPER_ID_IDENTITY is set): roostctl, then the Sparkle
#      chain (codesign_sparkle_or_die — strict inner→outer, never
#      --deep), then the outer app.
#
# Sparkle posture (6c: mechanics shipped, feed deliberately absent):
# the bundle carries the signed framework and the updater machinery,
# but NO SUFeedURL and NO SUPublicEDKey — a build with no feed never
# checks, and the two Roost bundles must never be able to offer each
# other's updates (see Info-iced.plist.template's header). Feed
# enablement is the ROOST_ICED_SPARKLE_FEED_URL +
# ROOST_ICED_SPARKLE_ED_PUBLIC_KEY env pair below (both set → keys
# PlistBuddy-inserted; exactly one → hard error; neither → today's
# posture).
#
# What this script deliberately does NOT do (unlike bundle.sh):
#   * No SwiftPM build, no SwiftPM resource-bundle copy — the iced
#     chrome fonts are `include_bytes!`'d and the themes ship
#     compiled into `roost-ui-model`; nothing is loaded from a
#     resource bundle at runtime.
#
# Deferred the same way bundle.sh defers them (shared posture, not a
# difference):
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
# .deb, and `roostctl identify` do.
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

CARGO_BIN="$(roost_find_cargo)"
roost_setup_cargo_profile "${CONFIG}"

echo "==> Building roost-iced (cargo build -p roost-iced --${ROOST_CARGO_PROFILE_DIR})"
(
  cd "${REPO_ROOT}"
  # shellcheck disable=SC2086  # ROOST_CARGO_PROFILE_FLAG must word-split (empty => no flag)
  "${CARGO_BIN}" build -p roost-iced ${ROOST_CARGO_PROFILE_FLAG}
)

CARGO_TARGET="$(roost_cargo_target_dir "${REPO_ROOT}")"
ICED_BUILD_BIN="${CARGO_TARGET}/${ROOST_CARGO_PROFILE_DIR}/roost-iced"
if [ ! -x "${ICED_BUILD_BIN}" ]; then
  echo "error: cargo build did not produce ${ICED_BUILD_BIN}" >&2
  exit 1
fi

roost_assemble_skeleton "${APP_DIR}"

cp "${ICED_BUILD_BIN}" "${APP_DIR}/Contents/MacOS/${APP_NAME}"
chmod +x "${APP_DIR}/Contents/MacOS/${APP_NAME}"

roost_stamp_plist "${TEMPLATE_PLIST}" "${APP_DIR}" "${VERSION}"
# Optional feed enablement (plan 028 § 3.9): both env vars set inserts
# SUFeedURL + SUPublicEDKey into the stamped plist; exactly one is a
# hard error; neither (the default) keeps today's no-feed posture.
roost_insert_sparkle_feed "${APP_DIR}" \
  "${ROOST_ICED_SPARKLE_FEED_URL:-}" \
  "${ROOST_ICED_SPARKLE_ED_PUBLIC_KEY:-}"
roost_write_pkginfo "${APP_DIR}"

roost_install_app_icon "${ICON_COMPOSER_SRC}" "${ICON_SRC}" "${APP_DIR}"

# M8-parity: embed roostctl under Contents/Resources/bin/ so `claude
# install` invoked from inside Roost-Iced.app writes hook paths that
# point at the bundled binary, not a dev-machine target/ path.
roost_build_and_embed_roostctl "${REPO_ROOT}" "${APP_DIR}" "${CONFIG}"

# Sparkle.framework embed (M6 6c mechanics — plan 028 § 3.10). Fetch is
# pinned-version + SHA-verified and cached (idempotent stamp); cp -R
# preserves the Versions/ symlink farm codesign requires. This stage is
# deliberately OUTSIDE the signing conditional below: a
# ROOST_ALLOW_UNSIGNED=1 build must still ship the framework — only the
# signing is conditional, never the bundle's contents.
"${REPO_ROOT}/third_party/sparkle/fetch.sh"
SPARKLE_FW_SRC="${REPO_ROOT}/third_party/sparkle/out/Sparkle.framework"
echo "==> Embedding Sparkle.framework"
mkdir -p "${APP_DIR}/Contents/Frameworks"
# roost_assemble_skeleton wiped APP_DIR, so the destination can't
# pre-exist today — but `cp -R` onto an existing directory would nest a
# second Sparkle.framework inside it; the delete keeps this stage
# deterministic on its own, not by courtesy of the helper's ordering.
rm -rf "${APP_DIR}/Contents/Frameworks/Sparkle.framework"
cp -R "${SPARKLE_FW_SRC}" "${APP_DIR}/Contents/Frameworks/Sparkle.framework"

# Signing. When ROOST_DEVELOPER_ID_IDENTITY is set (release CI, or a dev who
# holds the cert) we sign with that Developer ID + a secure `--timestamp` so the
# bundle can be notarized. Otherwise we fall back to ad-hoc (`-`) signing: fine
# for local launch, but Gatekeeper will warn and notarization is impossible.
# The inner→outer order is required — codesign seals nested code into the
# outer signature: embedded roostctl, then the Sparkle chain (itself strictly
# inner→outer — see codesign_sparkle_or_die in bundle-lib.sh), then the .app.
ENT_FILE="${MAC_DIR}/Resources/Roost-Iced.entitlements"
# The bundled roostctl helper gets the same narrower entitlements file
# bundle.sh uses: it never records audio/video or sends Apple events,
# so it must not inherit the app's capture entitlements.
ROOSTCTL_ENT_FILE="${MAC_DIR}/Resources/roostctl.entitlements"
if roost_setup_signing "${ENT_FILE}" "${ROOSTCTL_ENT_FILE}"; then
  codesign_or_die "${APP_DIR}/Contents/Resources/bin/roostctl" "${ROOSTCTL_ENT_FILE}"
  # An abandoned Sparkle chain (a component failure bypassed by
  # ROOST_ALLOW_UNSIGNED=1) returns nonzero: skip the outer signature
  # too, so a half-re-signed framework is never sealed under it — the
  # exact state the strict chain exists to prevent. The hard-fail path
  # (no ROOST_ALLOW_UNSIGNED) exits inside the helper and never gets
  # here.
  if codesign_sparkle_or_die "${APP_DIR}/Contents/Frameworks/Sparkle.framework"; then
    codesign_or_die "${APP_DIR}"
  else
    echo "==> warn: Sparkle chain abandoned; skipping the outer app signature so a half-re-signed framework is never sealed under it" >&2
  fi
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
