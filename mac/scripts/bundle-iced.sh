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
#      bundle.sh) and the `roost-session` daemon under Contents/MacOS/,
#      with a relative compatibility symlink beside roostctl — HS-4b
#      mechanics (plan 041 § 3.2).
#   6. Fetches (pinned version+SHA, cached — third_party/sparkle/
#      fetch.sh) and embeds Sparkle.framework under Contents/
#      Frameworks/ — M6 6c mechanics (plan 028). The embed runs even
#      for ROOST_ALLOW_UNSIGNED=1 builds; only the signing is
#      conditional.
#   7. Code-signs (ad-hoc by default, Developer ID when
#      ROOST_DEVELOPER_ID_IDENTITY is set): roostctl and roost-session,
#      then the Sparkle chain (codesign_sparkle_or_die — strict
#      inner→outer, never --deep), then the outer app — which is skipped
#      if either nested binary or the Sparkle chain did not actually get
#      our signature, so a bad nested state is never sealed.
#   8. Self-checks the assembled bundle (both embedded binaries, the
#      symlink's target, a working packaged invocation, and — when the
#      outer signature actually got written — a strict/deep codesign
#      verify plus proof that the nested signatures are OURS). Nonzero
#      exit on any miss.
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

# HS-4b (plan 041 § 3.2): ship the host-session daemon inside the app so
# macOS can start a local session with nothing else installed. The helper
# documents why it lands in Contents/MacOS/ with a symlink beside roostctl.
roost_build_and_embed_roost_session "${REPO_ROOT}" "${APP_DIR}" "${CONFIG}"

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
# outer signature: embedded roostctl and roost-session, then the Sparkle chain
# (itself strictly inner→outer — see codesign_sparkle_or_die in bundle-lib.sh),
# then the .app.
ENT_FILE="${MAC_DIR}/Resources/Roost-Iced.entitlements"
# The bundled roostctl helper gets the same narrower entitlements file
# bundle.sh uses: it never records audio/video or sends Apple events,
# so it must not inherit the app's capture entitlements.
ROOSTCTL_ENT_FILE="${MAC_DIR}/Resources/roostctl.entitlements"
# The nested roost-session daemon gets an empty entitlements dict: the
# hardened runtime comes from --options runtime, and the daemon needs no
# hole punched back through it (see the file's own comment).
SESSION_ENT_FILE="${MAC_DIR}/Resources/roost-session.entitlements"

# roost_setup_signing preflights the app's + roostctl's entitlements files;
# its signature is shared with the Swift bundle.sh (no daemon there) and must
# not grow a third, so this file is preflighted here with the same semantics.
# On the bypass branch nothing gets signed at all — an unsigned Mach-O in
# Contents/MacOS/ sealed under a signed .app fails codesign --verify --deep,
# so a partially-signed bundle is worse than an unsigned one.
SESSION_ENT_PRESENT=1
if [ ! -f "${SESSION_ENT_FILE}" ]; then
  if [ "${ROOST_ALLOW_UNSIGNED:-0}" = "1" ]; then
    echo "==> warn: missing entitlements file (${SESSION_ENT_FILE}); ROOST_ALLOW_UNSIGNED=1 set, shipping unsigned"
    SESSION_ENT_PRESENT=0
  else
    echo "error: missing entitlements file (${SESSION_ENT_FILE}) (set ROOST_ALLOW_UNSIGNED=1 to bypass)" >&2
    exit 1
  fi
fi

if [ "${SESSION_ENT_PRESENT}" = "1" ] && roost_setup_signing "${ENT_FILE}" "${ROOSTCTL_ENT_FILE}"; then
  codesign_or_die "${APP_DIR}/Contents/Resources/bin/roostctl" "${ROOSTCTL_ENT_FILE}"
  # The canonical Mach-O, not the Resources/bin symlink that points at it.
  codesign_or_die "${APP_DIR}/Contents/MacOS/roost-session" "${SESSION_ENT_FILE}"

  # Under ROOST_ALLOW_UNSIGNED=1 a nested sign that FAILED returns 0 and
  # the build walks on. The outer sign is not --deep, so it would then
  # seal a helper still wearing only its linker signature — nested code
  # with no hardened runtime and (with a Developer ID) the wrong Team ID,
  # which notarization and Gatekeeper can reject. Same disposition as the
  # abandoned-Sparkle branch below: refuse to seal a bad nested state.
  NESTED_SIGNED=1
  for nested_bin in \
    "${APP_DIR}/Contents/Resources/bin/roostctl" \
    "${APP_DIR}/Contents/MacOS/roost-session"
  do
    if ! roost_is_runtime_signed "${nested_bin}"; then
      echo "==> warn: ${nested_bin} does not carry our signature (no hardened-runtime flag)" >&2
      NESTED_SIGNED=0
    fi
  done

  # An abandoned Sparkle chain (a component failure bypassed by
  # ROOST_ALLOW_UNSIGNED=1) returns nonzero: skip the outer signature
  # too, so a half-re-signed framework is never sealed under it — the
  # exact state the strict chain exists to prevent. The hard-fail path
  # (no ROOST_ALLOW_UNSIGNED) exits inside the helper and never gets
  # here.
  if [ "${NESTED_SIGNED}" != "1" ]; then
    echo "==> warn: a nested binary is not signed by us; skipping the Sparkle chain and the outer app signature so an unsigned-by-us helper is never sealed under them" >&2
  elif codesign_sparkle_or_die "${APP_DIR}/Contents/Frameworks/Sparkle.framework"; then
    codesign_or_die "${APP_DIR}"
  else
    echo "==> warn: Sparkle chain abandoned; skipping the outer app signature so a half-re-signed framework is never sealed under it" >&2
  fi
fi

# ---------------------------------------------------------------------
# Bundle self-check (plan 041 § 3.2)
#
# Assembly stages that silently drop their output used to ship green:
# nothing — not CI, not the release job — asserted that roostctl was
# actually embedded, so a bundle missing it would have merged and
# shipped. This proves the bundle contains what the stages above were
# supposed to put in it, on EVERY assemble (local, CI, release). Kept
# cheap and network-free: CI's macOS iced cells run this script twice
# (keyless, then test-keyed).
# ---------------------------------------------------------------------
roost_iced_bundle_self_check() {
  local app_dir="$1"
  local app_name="$2"
  local failed=0

  echo "==> Self-check: ${app_dir}"

  local main_exe="${app_dir}/Contents/MacOS/${app_name}"
  if [ -f "${main_exe}" ] && [ -x "${main_exe}" ]; then
    echo "    OK: Contents/MacOS/${app_name} present and executable"
  else
    echo "    FAIL: Contents/MacOS/${app_name} missing or not executable" >&2
    failed=1
  fi

  # The REAL Mach-O must live here, not another symlink: this is the
  # path the UI's sibling-of-exe discovery rung searches, the path the
  # launchd recipe names, and the path the outer signature seals.
  local session_bin="${app_dir}/Contents/MacOS/roost-session"
  if [ -f "${session_bin}" ] && [ ! -L "${session_bin}" ] && [ -x "${session_bin}" ]; then
    echo "    OK: Contents/MacOS/roost-session present, a regular file, executable"
  else
    echo "    FAIL: Contents/MacOS/roost-session missing, not a regular file, or not executable" >&2
    failed=1
  fi

  local ctl_bin="${app_dir}/Contents/Resources/bin/roostctl"
  if [ -f "${ctl_bin}" ] && [ -x "${ctl_bin}" ]; then
    echo "    OK: Contents/Resources/bin/roostctl present and executable"
  else
    echo "    FAIL: Contents/Resources/bin/roostctl missing or not executable" >&2
    failed=1
  fi

  local session_link="${app_dir}/Contents/Resources/bin/roost-session"
  local want_target="../../MacOS/roost-session"
  local got_target=""
  if [ -L "${session_link}" ]; then
    got_target="$(readlink "${session_link}")"
  fi
  # Exact target, not merely "resolves": an absolute link would work on
  # this machine and break the moment the bundle is copied into
  # /Applications or mounted from a DMG.
  if [ "${got_target}" = "${want_target}" ]; then
    echo "    OK: Contents/Resources/bin/roost-session -> ${want_target}"
  else
    echo "    FAIL: Contents/Resources/bin/roost-session is not a symlink to ${want_target} (readlink gave '${got_target}')" >&2
    failed=1
  fi

  # Resolution is not execution. Run the daemon THROUGH the symlink,
  # exactly as `roostctl session start` will from its sibling dir.
  # `identify` is purpose-built for this: one JSON identity line on
  # stdout, no socket, no profile, no side effects.
  if "${session_link}" identify >/dev/null 2>&1; then
    echo "    OK: 'Contents/Resources/bin/roost-session identify' runs through the symlink"
  else
    echo "    FAIL: 'Contents/Resources/bin/roost-session identify' did not exit 0" >&2
    failed=1
  fi

  # Whether to verify signatures is keyed on what actually HAPPENED, not
  # on ROOST_ALLOW_UNSIGNED — that flag only *tolerates* signing
  # failures; with working inputs a build with it set still signs
  # everything, and skipping verification there would be a silent pass.
  #
  # The predicate is Contents/_CodeSignature/CodeResources, which only
  # the outer bundle signature writes. `codesign -dv` cannot be the
  # predicate: it reports a signature on a bundle that was never sealed
  # (linker signing — see roost_is_runtime_signed in bundle-lib.sh).
  if [ -f "${app_dir}/Contents/_CodeSignature/CodeResources" ]; then
    if ! codesign -dv "${app_dir}" >/dev/null 2>&1; then
      # Sealed but unreadable is BROKEN, not unsigned — something wrote
      # the resource seal and the signature no longer reads back. Fatal
      # regardless of ROOST_ALLOW_UNSIGNED: that bypass exists only for a
      # bundle that was genuinely never signed at all.
      echo "    FAIL: the .app carries Contents/_CodeSignature/CodeResources but 'codesign -dv' cannot read its signature — the bundle is broken, not unsigned" >&2
      failed=1
    else
      # Both nested binaries must carry OUR signature. A strict verify
      # alone does not prove that — see roost_is_runtime_signed in
      # bundle-lib.sh for why the hardened-runtime flag is the only
      # honest discriminator here.
      local nested
      for nested in "${session_bin}" "${ctl_bin}"; do
        if codesign --verify --strict "${nested}" >/dev/null 2>&1 \
           && roost_is_runtime_signed "${nested}"; then
          echo "    OK: ${nested#"${app_dir}/"} strictly verifies and carries our hardened-runtime signature"
        else
          echo "    FAIL: ${nested#"${app_dir}/"} does not strictly verify, or lacks the hardened-runtime flag that proves we signed it" >&2
          failed=1
        fi
      done
      # Signing is inner→outer and never --deep; *verification* is deep —
      # it is the only way to prove the nested signatures survived being
      # sealed under the outer one (same check CI's "Assert bundle
      # contents" step runs).
      if codesign --verify --deep --strict "${app_dir}" >/dev/null 2>&1; then
        echo "    OK: codesign --verify --deep --strict passed for the .app"
      else
        echo "    FAIL: codesign --verify --deep --strict failed for the .app" >&2
        failed=1
      fi
    fi
  elif [ "${ROOST_ALLOW_UNSIGNED:-0}" = "1" ]; then
    echo "    WARN: the .app carries no outer signature — SKIPPING all signature verification (ROOST_ALLOW_UNSIGNED=1 bypassed a signing step). This bundle is not shippable." >&2
  else
    echo "    FAIL: the .app carries no outer signature and ROOST_ALLOW_UNSIGNED is not set" >&2
    failed=1
  fi

  if [ "${failed}" -ne 0 ]; then
    echo "error: bundle self-check failed for ${app_dir}" >&2
    exit 1
  fi
  echo "    Self-check passed."
}

roost_iced_bundle_self_check "${APP_DIR}" "${APP_NAME}"

echo "==> Bundled: ${APP_DIR}"
echo "    Bundle ID:       ${BUNDLE_ID}"
echo "    Version:         ${VERSION}"
echo "    Executable:      ${APP_DIR}/Contents/MacOS/${APP_NAME}"
echo "    Embedded CLI:    ${APP_DIR}/Contents/Resources/bin/roostctl"
echo "    Embedded daemon: ${APP_DIR}/Contents/MacOS/roost-session"
echo
echo "Note: with both Roost.app and Roost-Iced.app installed, 'claude"
echo "install' points Claude's hook file at whichever bundle's roostctl"
echo "ran it last (roadmap 6a decision — noted, not fixed here)."
echo
echo "Launch with: open '${APP_DIR}'"
