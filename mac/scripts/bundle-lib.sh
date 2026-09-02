# shellcheck shell=bash
#
# Shared toolkit-agnostic bundling stages for mac/scripts/bundle.sh and
# mac/scripts/bundle-iced.sh.
#
# Sourced, not executed: callers set `set -euo pipefail` themselves and
# source this file after computing SCRIPT_DIR/MAC_DIR/REPO_ROOT. Every
# function here is parameterized — no assumptions about app name, bundle
# dir, or entitlements paths baked in as globals.

# roost_check_libghostty_archive REPO_ROOT
#
# Sanity check: the static libghostty-vt archive must exist or `swift
# build` / `cargo build` will fail at the linker. The same precondition
# the Mac README documents.
roost_check_libghostty_archive() {
  local repo_root="$1"
  if [ ! -f "${repo_root}/third_party/ghostty/out/lib/libghostty-vt.a" ]; then
    echo "error: libghostty-vt static archive not built." >&2
    echo "       Run: ${repo_root}/third_party/ghostty/build.sh" >&2
    exit 1
  fi
}

# roost_workspace_version REPO_ROOT
#
# Prints the marketing version derived from the workspace's single
# source of truth — `[workspace.package].version` in Cargo.toml — so a
# local bundle build reports the same version the iced UI, the .deb, and
# `roostctl identify` do. Callers should let $ROOST_VERSION override the
# result. The `^version` anchor matches only the top-level key, not the
# `version = "…"` entries nested under `[workspace.dependencies]`.
roost_workspace_version() {
  local repo_root="$1"
  local cargo_version
  # Explicit failure check: command substitution suppresses errexit in
  # bash 3.2, so without it a missing/unreadable version line would
  # silently become 0.0.0 where the original inline pipeline aborted
  # under pipefail.
  cargo_version="$(grep -E '^version[[:space:]]*=' "${repo_root}/Cargo.toml" | head -1 \
    | sed -E 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/')" || {
    echo "error: failed to read workspace version from ${repo_root}/Cargo.toml" >&2
    return 1
  }
  echo "${cargo_version:-0.0.0}"
}

# roost_assemble_skeleton APP_DIR
#
# Fresh bundle skeleton: Contents/MacOS + Contents/Resources. Both
# bundle scripts wipe and recreate on every run (no incremental
# state) so a stale bundle can't wear a previous build's leftovers.
roost_assemble_skeleton() {
  local app_dir="$1"
  echo "==> Assembling ${app_dir}"
  rm -rf "${app_dir}"
  mkdir -p "${app_dir}/Contents/MacOS"
  mkdir -p "${app_dir}/Contents/Resources"
}

# roost_stamp_plist TEMPLATE_PLIST APP_DIR VERSION
#
# Info.plist with version substitution. `sed -e s/.../.../g` is
# portable across BSD + GNU sed; quoting `@VERSION@` and using a
# unique-enough sentinel keeps the substitution unambiguous.
roost_stamp_plist() {
  local template_plist="$1"
  local app_dir="$2"
  local version="$3"
  echo "==> Stamping Info.plist (version=${version})"
  sed -e "s/@VERSION@/${version}/g" "${template_plist}" \
    > "${app_dir}/Contents/Info.plist"
}

# roost_insert_sparkle_feed APP_DIR FEED_URL ED_PUBLIC_KEY
#
# Optional Sparkle feed enablement for the iced bundle (plan 028 § 3.9).
# The template plist deliberately ships NO SUFeedURL/SUPublicEDKey — the
# feed-enabled configuration is opt-in at bundle time via the
# ROOST_ICED_SPARKLE_FEED_URL + ROOST_ICED_SPARKLE_ED_PUBLIC_KEY env
# pair, which bundle-iced.sh passes through here. Both empty is today's
# posture (no keys inserted, byte-identical plist); both set inserts the
# pair into the already-stamped Info.plist; exactly one set is a hard
# error — a feed URL without its EdDSA public key (or vice versa) is a
# misconfigured build that would either never verify an update or never
# check at all, and must not sign+ship looking healthy.
#
# Call after roost_stamp_plist (this edits the stamped Info.plist, not
# the template).
roost_insert_sparkle_feed() {
  local app_dir="$1"
  local feed_url="$2"
  local ed_public_key="$3"
  local plist="${app_dir}/Contents/Info.plist"

  if [ -z "${feed_url}" ] && [ -z "${ed_public_key}" ]; then
    return 0
  fi
  if [ -z "${feed_url}" ] || [ -z "${ed_public_key}" ]; then
    echo "error: ROOST_ICED_SPARKLE_FEED_URL and ROOST_ICED_SPARKLE_ED_PUBLIC_KEY" >&2
    echo "       must be set together (both, or neither) — a feed without its" >&2
    echo "       public key (or vice versa) is a misconfigured Sparkle build." >&2
    exit 1
  fi
  # PlistBuddy parses its -c string itself: an embedded double quote
  # unbalances it, and PlistBuddy then exits 0 on the parse error — so
  # under set -e the build would continue with only ONE key inserted,
  # silently violating the both-or-neither contract. No legitimate feed
  # URL or base64 EdDSA key contains a quote; reject rather than escape.
  case "${feed_url}${ed_public_key}" in
    *\"*)
      echo "error: Sparkle feed URL / public key must not contain double quotes" >&2
      exit 1
      ;;
  esac
  echo "==> Inserting Sparkle feed keys (SUFeedURL=${feed_url})"
  /usr/libexec/PlistBuddy -c "Add :SUFeedURL string ${feed_url}" "${plist}"
  /usr/libexec/PlistBuddy -c "Add :SUPublicEDKey string ${ed_public_key}" "${plist}"
}

# roost_write_pkginfo APP_DIR
#
# Classic four-byte PkgInfo so Finder recognizes the bundle type
# without leaning on Info.plist alone. macOS tolerates a missing
# PkgInfo nowadays but Spotlight prefers it.
roost_write_pkginfo() {
  local app_dir="$1"
  printf "APPL????" > "${app_dir}/Contents/PkgInfo"
}

# roost_install_app_icon ICON_COMPOSER_SRC ICON_SRC APP_DIR
#
# App icon. On macOS 26 (Tahoe) a loose .icns is treated as legacy and
# inset on the system glass tile (a gray frame around the art). The fix
# is a compiled Icon Composer catalog (generated by
# packaging/icon/generate_icons.py) — `actool` renders it into
# Assets.car + a flattened AppIcon.icns, and Tahoe then fills the tile
# edge-to-edge (parity with ghostty/cmux). `actool` ships with full
# Xcode, not the bare Command Line Tools, so we fall back to the
# committed flat .icns when it's unavailable — that still builds a
# launchable bundle, just with the framed legacy icon on Tahoe.
# CFBundleIconName=AppIcon (set in the Info.plist template) routes the
# OS to the catalog; the .icns covers pre-Tahoe and the no-actool path.
#
# Reads LSMinimumSystemVersion back from APP_DIR's already-stamped
# Info.plist, so the caller must stamp Info.plist before calling this.
roost_install_app_icon() {
  local icon_composer_src="$1"
  local icon_src="$2"
  local app_dir="$3"

  local icon_done=0
  if [ -d "${icon_composer_src}" ] && command -v xcrun >/dev/null 2>&1 \
     && xcrun --find actool >/dev/null 2>&1; then
    # Match the bundle's own minimum OS so the catalog targets what the app ships.
    local min_os
    min_os="$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' \
      "${app_dir}/Contents/Info.plist" 2>/dev/null || echo 26.0)"
    local actool_tmp
    actool_tmp="$(mktemp -d)"
    echo "==> Compiling AppIcon.icon with actool (Tahoe glass icon, min ${min_os})"
    if xcrun actool "${icon_composer_src}" \
         --compile "${actool_tmp}" \
         --platform macosx \
         --minimum-deployment-target "${min_os}" \
         --app-icon AppIcon \
         --output-partial-info-plist "${actool_tmp}/partial.plist" \
         --errors --warnings >/dev/null 2>&1 \
       && [ -f "${actool_tmp}/Assets.car" ]; then
      cp "${actool_tmp}/Assets.car" "${app_dir}/Contents/Resources/Assets.car"
      # actool also emits a flattened .icns — keep it as the pre-Tahoe fallback.
      [ -f "${actool_tmp}/AppIcon.icns" ] \
        && cp "${actool_tmp}/AppIcon.icns" "${app_dir}/Contents/Resources/AppIcon.icns"
      echo "    Compiled: ${app_dir}/Contents/Resources/Assets.car"
      icon_done=1
    else
      echo "    warn: actool failed; falling back to flat AppIcon.icns" >&2
    fi
    rm -rf "${actool_tmp}"
  fi
  if [ "${icon_done}" -eq 0 ]; then
    if [ -f "${icon_src}" ]; then
      echo "==> Including flat AppIcon.icns (no actool — Tahoe will show the framed legacy icon)"
      cp "${icon_src}" "${app_dir}/Contents/Resources/AppIcon.icns"
    else
      echo "==> No app icon found; bundle ships without a custom icon"
    fi
  fi
}

# roost_find_cargo
#
# Prints the cargo to invoke. Discovers `cargo` on PATH instead of
# hardcoding ~/.cargo/bin/cargo — release runners may have cargo at a
# different prefix (toolchain managed by mise / rustup / system
# package). Falling back to the literal path preserves the prior
# behavior for the common dev case.
roost_find_cargo() {
  local cargo_bin
  cargo_bin="$(command -v cargo || true)"
  if [ -z "${cargo_bin}" ] && [ -x "${HOME}/.cargo/bin/cargo" ]; then
    cargo_bin="${HOME}/.cargo/bin/cargo"
  fi
  if [ -z "${cargo_bin}" ]; then
    echo "error: cargo not found on PATH or at ~/.cargo/bin/cargo" >&2
    exit 1
  fi
  echo "${cargo_bin}"
}

# roost_setup_cargo_profile CONFIG
#
# Sets ROOST_CARGO_PROFILE_FLAG (word-split deliberately; empty for
# debug) and ROOST_CARGO_PROFILE_DIR in the caller's scope — the same
# caller-scope convention roost_setup_signing uses.
roost_setup_cargo_profile() {
  ROOST_CARGO_PROFILE_FLAG="--release"
  ROOST_CARGO_PROFILE_DIR="release"
  if [ "$1" = "debug" ]; then
    ROOST_CARGO_PROFILE_FLAG=""
    ROOST_CARGO_PROFILE_DIR="debug"
  fi
}

# roost_cargo_target_dir REPO_ROOT
#
# Prints the artifact root, honoring CARGO_TARGET_DIR — shared caches
# (e.g. sccache + CI matrices that fan out across configs) routinely
# override the default `<repo>/target/` location. Cargo resolves a
# relative CARGO_TARGET_DIR from its own CWD (the repo root, where the
# build subshells cd) — discovery anchors the same way so the two
# can't diverge.
roost_cargo_target_dir() {
  local repo_root="$1"
  local cargo_target="${CARGO_TARGET_DIR:-${repo_root}/target}"
  case "${cargo_target}" in
    /*) ;;
    *) cargo_target="${repo_root}/${cargo_target}" ;;
  esac
  echo "${cargo_target}"
}

# roost_build_workspace_bin REPO_ROOT CONFIG PACKAGE BIN_NAME
#
# Builds one workspace binary for CONFIG and sets ROOST_BUILT_BIN to the
# artifact path in the caller's scope — the same caller-scope convention
# roost_setup_cargo_profile uses, and required here rather than printing
# the path: a command substitution would swallow cargo's build output
# and demote the missing-artifact `exit 1` to a subshell exit.
roost_build_workspace_bin() {
  local repo_root="$1"
  local config="$2"
  local package="$3"
  local bin_name="$4"

  local cargo_bin
  cargo_bin="$(roost_find_cargo)"
  roost_setup_cargo_profile "${config}"
  echo "==> Building ${bin_name} (cargo build -p ${package} --${ROOST_CARGO_PROFILE_DIR})"
  (
    # shellcheck disable=SC2164  # callers run this lib under `set -e`
    cd "${repo_root}"
    # shellcheck disable=SC2086
    "${cargo_bin}" build -p "${package}" ${ROOST_CARGO_PROFILE_FLAG}
  )

  ROOST_BUILT_BIN="$(roost_cargo_target_dir "${repo_root}")/${ROOST_CARGO_PROFILE_DIR}/${bin_name}"
  if [ ! -x "${ROOST_BUILT_BIN}" ]; then
    echo "error: cargo build did not produce ${ROOST_BUILT_BIN}" >&2
    exit 1
  fi
}

# roost_build_and_embed_roostctl REPO_ROOT APP_DIR CONFIG
#
# Embed roostctl under Contents/Resources/bin/ so `claude install`
# invoked from inside the bundled app writes hook paths that point at
# the bundled binary, not a dev-machine target/ path. The CLI build is
# fast and tracked through the same Cargo cache as any cargo build
# invocation; rebuilding here keeps the bundle in lockstep with
# whatever roost-cli source the developer has checked out.
roost_build_and_embed_roostctl() {
  local repo_root="$1"
  local app_dir="$2"
  local config="$3"

  roost_build_workspace_bin "${repo_root}" "${config}" roost-cli roostctl
  mkdir -p "${app_dir}/Contents/Resources/bin"
  cp "${ROOST_BUILT_BIN}" "${app_dir}/Contents/Resources/bin/roostctl"
  chmod +x "${app_dir}/Contents/Resources/bin/roostctl"
  echo "    Embedded: ${app_dir}/Contents/Resources/bin/roostctl"
}

# roost_build_and_embed_roost_session REPO_ROOT APP_DIR CONFIG
#
# Embed the `roost-session` host-session daemon (HS-4b, plan 041) so a
# macOS bundle can start a local session with no separately installed
# binary.
#
# Two paths, one Mach-O:
#   * The REAL binary lands in Contents/MacOS/ — Apple's documented
#     helper-tool location, and the directory the iced UI's own
#     sibling-of-exe discovery rung searches (it passes its own
#     executable to locate_session_binary, whose sibling dir inside a
#     bundle is Contents/MacOS/). It is also the stable path the
#     launchd recipe in docs/guides/host-sessions.md names.
#   * A RELATIVE symlink at Contents/Resources/bin/roost-session,
#     because the *bundled roostctl* sits there and `roostctl session
#     start` climbs the same ladder from its own sibling dir. Relative
#     so it survives the bundle being moved, copied into
#     /Applications, or mounted read-only from a DMG.
roost_build_and_embed_roost_session() {
  local repo_root="$1"
  local app_dir="$2"
  local config="$3"

  roost_build_workspace_bin "${repo_root}" "${config}" roost-session roost-session
  cp "${ROOST_BUILT_BIN}" "${app_dir}/Contents/MacOS/roost-session"
  chmod +x "${app_dir}/Contents/MacOS/roost-session"
  mkdir -p "${app_dir}/Contents/Resources/bin"
  ln -sfn ../../MacOS/roost-session "${app_dir}/Contents/Resources/bin/roost-session"
  echo "    Embedded: ${app_dir}/Contents/MacOS/roost-session"
  echo "    Embedded: ${app_dir}/Contents/Resources/bin/roost-session -> ../../MacOS/roost-session"
}

# roost_is_runtime_signed PATH
#
# True only when PATH carries a signature WE wrote — one with the
# hardened-runtime flag, which `codesign --options runtime`
# (codesign_or_die) sets and nothing else does.
#
# Why a flag check rather than a verify: on Apple Silicon the linker
# ad-hoc-signs every arm64 Mach-O it emits, so a raw `cargo build`
# artifact already reports flags=0x20002(adhoc,linker-signed), ALREADY
# passes `codesign --verify --strict`, and yields a ZERO-byte
# `codesign -d --entitlements` extraction. Neither a clean verify nor an
# empty entitlements dump can therefore distinguish "we signed it" from
# "we never touched it". The `runtime` token can: the linker never sets
# it, and `linker-signed` must not be mistaken for it — hence the exact
# comma-delimited token match below rather than a substring grep.
roost_is_runtime_signed() {
  local target="$1"
  local flags=""
  # codesign -d writes to stderr; a wholly unsigned target yields no
  # flags= line at all, which falls through to the failure case.
  flags="$(codesign -dvv "${target}" 2>&1 \
    | sed -n 's/.*flags=0x[0-9a-fA-F]*(\([^)]*\)).*/\1/p' | head -1)" || true
  case ",${flags}," in
    *,runtime,*) return 0 ;;
    *) return 1 ;;
  esac
}

# roost_setup_signing ENT_FILE ROOSTCTL_ENT_FILE
#
# Signing setup. When ROOST_DEVELOPER_ID_IDENTITY is set (release CI, or
# a dev who holds the cert) we sign with that Developer ID + a secure
# `--timestamp` so the bundle can be notarized. Otherwise we fall back
# to ad-hoc (`-`) signing: fine for local launch, but Gatekeeper will
# warn and notarization is impossible.
#
# Failure handling: a botched signature is release-blocking (Gatekeeper
# reject, notarization fail, quarantined installs). Default is fail
# hard; the `ROOST_ALLOW_UNSIGNED=1` env var bypasses for the rare dev
# case where Xcode CLT codesign is missing.
#
# Sets SIGN_IDENTITY, TS_FLAG, and ROOST_SIGN_ENT_FILE in the caller's
# scope, and defines the codesign_or_die / codesign_framework_or_die /
# codesign_sparkle_or_die functions (also in the caller's scope — this
# is bash, functions defined here are global to the sourcing script).
# ROOST_SIGN_ENT_FILE stays global (not a local) because
# codesign_or_die's default-entitlements argument reads it after this
# function has already returned. Returns 1 (without exiting) when
# signing must be skipped entirely because ROOST_ALLOW_UNSIGNED=1
# bypassed a missing entitlements file or missing codesign — callers
# should skip calling the sign functions in that case.
roost_setup_signing() {
  ROOST_SIGN_ENT_FILE="$1"
  local ent_file="$1"
  local roostctl_ent_file="$2"

  SIGN_IDENTITY="${ROOST_DEVELOPER_ID_IDENTITY:--}"
  # `--timestamp` only with a real identity; ad-hoc signing can't be
  # timestamped. Kept as a plain (unquoted-on-use) string so it expands
  # to nothing when empty — bash 3.2-safe (no empty-array expansion
  # under `set -u`).
  TS_FLAG=""
  if [ "${SIGN_IDENTITY}" != "-" ]; then
    TS_FLAG="--timestamp"
  fi

  if [ ! -f "${ent_file}" ] || [ ! -f "${roostctl_ent_file}" ]; then
    # The entitlements files are committed and carry the app's TCC
    # capabilities (mic/camera/Apple-events); a missing one must not
    # silently ship an unsigned bundle that drops them. Fail hard like
    # the missing-codesign case, unless the operator explicitly opts
    # into an unsigned build.
    if [ "${ROOST_ALLOW_UNSIGNED:-0}" = "1" ]; then
      echo "==> warn: missing entitlements file (${ent_file} or ${roostctl_ent_file}); ROOST_ALLOW_UNSIGNED=1 set, shipping unsigned"
      return 1
    else
      echo "error: missing entitlements file (${ent_file} or ${roostctl_ent_file}) (set ROOST_ALLOW_UNSIGNED=1 to bypass)" >&2
      exit 1
    fi
  elif ! command -v codesign >/dev/null 2>&1; then
    # codesign absent (no Xcode CLT). Honor the fail-hard intent above:
    # a missing signer would silently ship an unsigned bundle, so error
    # out unless the operator explicitly opts into an unsigned build.
    if [ "${ROOST_ALLOW_UNSIGNED:-0}" = "1" ]; then
      echo "==> warn: codesign not found; ROOST_ALLOW_UNSIGNED=1 set, shipping unsigned"
      return 1
    else
      echo "error: codesign not found (set ROOST_ALLOW_UNSIGNED=1 to bypass)" >&2
      exit 1
    fi
  fi

  if [ "${SIGN_IDENTITY}" = "-" ]; then
    echo "==> Ad-hoc codesign (set ROOST_DEVELOPER_ID_IDENTITY for a notarizable build)"
  else
    echo "==> Developer ID codesign (identity: ${SIGN_IDENTITY})"
  fi

  # shellcheck disable=SC2329  # invoked by the sourcing script after this function returns
  codesign_or_die() {
    local target="$1"
    # Optional per-target entitlements; defaults to the app's file. The
    # roostctl helper passes its own narrower file.
    local ent="${2:-${ROOST_SIGN_ENT_FILE}}"
    # shellcheck disable=SC2086  # TS_FLAG must word-split (empty => no flag)
    if codesign --force --sign "${SIGN_IDENTITY}" \
         --entitlements "${ent}" \
         --options runtime \
         ${TS_FLAG} \
         "${target}"
    then
      return 0
    fi
    if [ "${ROOST_ALLOW_UNSIGNED:-0}" = "1" ]; then
      echo "    warn: codesign(${target}) failed; ROOST_ALLOW_UNSIGNED=1 set, continuing"
      return 0
    fi
    echo "    error: codesign(${target}) failed (set ROOST_ALLOW_UNSIGNED=1 to bypass)" >&2
    exit 1
  }

  # Sparkle.framework is signed --deep but WITHOUT --entitlements: the
  # framework + its nested helpers (XPCServices/*.xpc, Updater.app,
  # Autoupdate) carry their own designated requirements, and forcing the
  # app's entitlements onto them can break Sparkle's XPC handshake. The
  # app's `disable-library-validation` entitlement (on the outer bundle)
  # is what lets it load this ad-hoc framework. --deep is safe here (a
  # framework signed with uniform options) — it is only dangerous on the
  # outer .app, where it would clobber these nested signatures.
  # shellcheck disable=SC2329  # invoked by the sourcing script after this function returns
  codesign_framework_or_die() {
    local target="$1"
    # shellcheck disable=SC2086  # TS_FLAG must word-split (empty => no flag)
    if codesign --force --sign "${SIGN_IDENTITY}" \
         --options runtime \
         --deep \
         ${TS_FLAG} \
         "${target}"
    then
      return 0
    fi
    if [ "${ROOST_ALLOW_UNSIGNED:-0}" = "1" ]; then
      echo "    warn: codesign(${target}) failed; ROOST_ALLOW_UNSIGNED=1 set, continuing"
      return 0
    fi
    echo "    error: codesign(${target}) failed (set ROOST_ALLOW_UNSIGNED=1 to bypass)" >&2
    exit 1
  }

  # One component of the strict Sparkle chain (codesign_sparkle_or_die
  # below). No --entitlements ever — Sparkle's helpers must keep their
  # own (empty, since Sparkle ≥2.6 removed its sandbox —
  # sparkle-project/Sparkle#2511) entitlements, not inherit the app's
  # TCC set. Extra per-component flags (Downloader.xpc's
  # --preserve-metadata=entitlements) arrive as additional arguments.
  #
  # Returns 1 (without exiting) when ROOST_ALLOW_UNSIGNED=1 bypassed a
  # failure: the caller must then ABANDON the remaining chain. Warning
  # per component and continuing would leave a half-Roost-signed chain
  # sealed under the outer signature — worse than a wholly vendor-signed
  # framework, and exactly the breaks-at-update-apply case the chain
  # exists to prevent.
  # shellcheck disable=SC2329  # invoked by codesign_sparkle_or_die after this function returns
  roost__codesign_sparkle_component() {
    local target="$1"
    shift
    # shellcheck disable=SC2086  # TS_FLAG must word-split (empty => no flag)
    if codesign --force --sign "${SIGN_IDENTITY}" \
         --options runtime \
         ${TS_FLAG} \
         "$@" \
         "${target}"
    then
      return 0
    fi
    if [ "${ROOST_ALLOW_UNSIGNED:-0}" = "1" ]; then
      echo "    warn: codesign(${target}) failed; ROOST_ALLOW_UNSIGNED=1 set — abandoning the rest of the Sparkle chain (partial re-signing breaks Sparkle at update-apply time)"
      return 1
    fi
    echo "    error: codesign(${target}) failed (set ROOST_ALLOW_UNSIGNED=1 to bypass)" >&2
    exit 1
  }

  # codesign_sparkle_or_die FRAMEWORK_PATH
  #
  # Signs an embedded Sparkle.framework via the strict inner→outer
  # chain, one component at a time, NO --deep anywhere:
  #
  #   Versions/B/XPCServices/Installer.xpc
  #   Versions/B/XPCServices/Downloader.xpc   (--preserve-metadata=entitlements)
  #   Versions/B/Autoupdate
  #   Versions/B/Updater.app
  #   the framework itself                     (no entitlements)
  #
  # then the caller signs the outer .app (with the app's entitlements)
  # after this returns — inner→outer to the end.
  #
  # This DELIBERATELY deviates from codesign_framework_or_die above,
  # whose comment says --deep is safe on a framework. That holds for
  # the Swift bundle's use (where it has shipped working updates and is
  # deliberately left as-is — Swift path untouched, recorded as a
  # future hygiene pass in plan 028 § 9), but the shed reference
  # embedding demonstrated the sharper truth: --deep, or a wrong
  # signing order, produces a bundle that signs AND notarizes clean yet
  # breaks at update-apply time — Sparkle's Installer/Downloader XPC
  # handshake rejects helpers whose signatures were clobbered from the
  # outside. Failure at the latest possible moment, on the user's
  # machine, mid-update. The iced path therefore adopts the strict
  # per-component chain. Downloader.xpc alone gets
  # --preserve-metadata=entitlements: Sparkle ≥2.6 removed its sandbox
  # (sparkle-project/Sparkle#2511), and preserving whatever
  # entitlements the component itself shipped keeps us correct across
  # Sparkle releases without ever stamping our own onto it.
  # shellcheck disable=SC2329  # invoked by the sourcing script after this function returns
  codesign_sparkle_or_die() {
    local fw="$1"
    local versions="${fw}/Versions/B"

    # The chain only protects what it actually signs — a missing
    # component means a truncated/flattened framework copy, and
    # signing the remainder would seal a broken bundle. Fail loudly.
    local component
    for component in \
      "${versions}/XPCServices/Installer.xpc" \
      "${versions}/XPCServices/Downloader.xpc" \
      "${versions}/Updater.app"
    do
      if [ ! -d "${component}" ]; then
        echo "    error: Sparkle component missing: ${component} (broken framework copy?)" >&2
        exit 1
      fi
    done
    if [ ! -f "${versions}/Autoupdate" ]; then
      echo "    error: Sparkle component missing: ${versions}/Autoupdate (broken framework copy?)" >&2
      exit 1
    fi

    echo "==> Signing Sparkle.framework (strict inner→outer chain, no --deep)"
    # A component failure under ROOST_ALLOW_UNSIGNED=1 returns 1: stop
    # the chain right there AND propagate the failure to the caller,
    # which must then skip the outer-app signature too — sealing a
    # half-re-signed framework under the outer signature is the exact
    # state this chain exists to prevent (see the helper's comment).
    roost__codesign_sparkle_component "${versions}/XPCServices/Installer.xpc" \
      && roost__codesign_sparkle_component "${versions}/XPCServices/Downloader.xpc" \
           --preserve-metadata=entitlements \
      && roost__codesign_sparkle_component "${versions}/Autoupdate" \
      && roost__codesign_sparkle_component "${versions}/Updater.app" \
      && roost__codesign_sparkle_component "${fw}"
  }

  return 0
}
