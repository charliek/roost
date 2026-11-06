#!/bin/sh
# Point this checkout's Android release signing at the Roost release key:
# symlinks mobile/android/key.properties (gitignored) to
# <secrets>/roost-android-key.properties. Prints no secret.
#
#   tools/mobile/link-signing.sh [secrets-dir]
#
# secrets-dir defaults to $ROOST_SECRETS_DIR, else .secrets/ in the main
# checkout (found through git's common dir, so a worktree gets it too). The
# properties file's storeFile must be absolute: Gradle resolves a relative one
# against mobile/android/app/, wherever the link points.
#
# Without the link, release builds are signed with the debug key
# (mobile/android/app/build.gradle.kts).
set -eu

fail() {
  echo "link-signing: $*" >&2
  exit 1
}

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
common=$(git -C "${repo}" rev-parse --path-format=absolute --git-common-dir)
secrets=${1:-${ROOST_SECRETS_DIR:-$(dirname "${common}")/.secrets}}
props="${secrets}/roost-android-key.properties"
[ -f "${props}" ] || fail "no ${props}"

for key in storeFile storePassword keyAlias keyPassword; do
  grep -q "^${key}=." "${props}" || fail "${props} has no ${key}"
done
store=$(sed -n 's/^storeFile=//p' "${props}")
case "${store}" in
  /*) ;;
  *) fail "storeFile in ${props} is not an absolute path" ;;
esac
[ -f "${store}" ] || fail "the keystore ${store} does not exist"

link="${repo}/mobile/android/key.properties"
git -C "${repo}" check-ignore -q mobile/android/key.properties ||
  fail "mobile/android/key.properties is not gitignored; refusing to link a secret there"
ln -sfn "${props}" "${link}"
echo "linked ${link} -> ${props}"
