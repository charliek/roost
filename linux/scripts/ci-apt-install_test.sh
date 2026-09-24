#!/usr/bin/env bash
# Test linux/scripts/ci-apt-install.sh against a fake sources dir/file and a
# fake `apt-get` on PATH, so the prune-then-update-then-install behavior is
# provable without sudo or a real apt run (plan 070 D5/C2, #529).
#
# The script under test always shells out to the real `apt-get`, so this
# test points it at a scratch sources dir/file via CI_APT_SOURCES_DIR /
# CI_APT_SOURCES_LIST (the script's own test-only override, documented in
# its header) and a fake `apt-get` shim placed first on PATH.
#
# Usage:
#   ./linux/scripts/ci-apt-install_test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_SCRIPT="${SCRIPT_DIR}/ci-apt-install.sh"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

pass() {
  printf 'ok: %s\n' "$*"
}

# Everything this test mutates — the fake sources dir/file, the fake
# apt-get's PATH shim, and its call log — lives under this one mktemp -d, so
# the cleanup trap can never reach anything outside it.
work_dir="$(mktemp -d)"
trap 'rm -rf "${work_dir}"' EXIT

bin_dir="${work_dir}/bin"
mkdir -p "${bin_dir}"
apt_log="${work_dir}/apt-calls.log"

# Logs one argument per line inside an @@CALL/@@END block (not `$*`, which
# would join argv with spaces and hide a word-split regression on an
# argument that legitimately contains one).
cat > "${bin_dir}/apt-get" <<'EOF'
#!/usr/bin/env bash
{
  echo "@@CALL apt-get"
  printf '%s\n' "$@"
  echo "@@END"
} >> "${FAKE_APT_LOG}"
if [ "${1:-}" = "update" ]; then
  ls -A "${CI_APT_SOURCES_DIR}" > "${FAKE_APT_SEEN_AT_UPDATE}"
  [ "${FAKE_APT_FAIL_UPDATE:-0}" = "1" ] && exit 1
fi
exit 0
EOF
chmod +x "${bin_dir}/apt-get"

export FAKE_APT_LOG="${apt_log}"
seen_at_update="${work_dir}/seen-at-update.txt"
export FAKE_APT_SEEN_AT_UPDATE="${seen_at_update}"
export PATH="${bin_dir}:${PATH}"

reset_apt_log() {
  : > "${apt_log}"
  : > "${seen_at_update}"
}

# deb822, the shape the runner images ship; the helper keys on `URIs:`.
write_ubuntu_sources() {
  printf 'Types: deb\nURIs: http://archive.ubuntu.com/ubuntu/\nSuites: noble\nComponents: main\n' > "$1"
}

# Asserts the call log holds exactly one `apt-get update -o
# Acquire::Retries=3` call followed by one `apt-get install -y <args...>`
# call, in that order — the shape every passing case below shares. Takes
# the expected install args as separate positional parameters (not a single
# pre-joined string) so an argument containing a space is compared as the
# one argument it is, never re-split.
assert_update_then_install() {
  local expected_file="${work_dir}/expected.log"
  {
    echo "@@CALL apt-get"
    echo "update"
    echo "-o"
    echo "Acquire::Retries=3"
    echo "@@END"
    echo "@@CALL apt-get"
    echo "install"
    echo "-y"
    printf '%s\n' "$@"
    echo "@@END"
  } > "${expected_file}"
  diff -u "${expected_file}" "${apt_log}" \
    || fail "apt-get call log did not match expected update-then-install shape (diff above: expected vs actual)"
}

# ---------------------------------------------------------------------
# Case 1: prunes non-Ubuntu entries and keeps ubuntu.sources.
# ---------------------------------------------------------------------
sources_dir1="${work_dir}/case1/sources.list.d"
sources_list1="${work_dir}/case1/sources.list"
mkdir -p "${sources_dir1}"
: > "${sources_list1}"
write_ubuntu_sources "${sources_dir1}/ubuntu.sources"
: > "${sources_dir1}/some-ppa.list"
: > "${sources_dir1}/another-vendor.sources"

reset_apt_log
out="$(CI_APT_SOURCES_DIR="${sources_dir1}" CI_APT_SOURCES_LIST="${sources_list1}" \
  "${INSTALL_SCRIPT}" libclang-dev 2>&1)"

[ -e "${sources_dir1}/ubuntu.sources" ] || fail "case1: ubuntu.sources was removed"
[ ! -e "${sources_dir1}/some-ppa.list" ] || fail "case1: some-ppa.list was not pruned"
[ ! -e "${sources_dir1}/another-vendor.sources" ] || fail "case1: another-vendor.sources was not pruned"
printf '%s\n' "${out}" | grep -q "some-ppa.list" || fail "case1: removal of some-ppa.list was not printed"
printf '%s\n' "${out}" | grep -q "another-vendor.sources" || fail "case1: removal of another-vendor.sources was not printed"
assert_update_then_install libclang-dev
[ "$(cat "${seen_at_update}")" = "ubuntu.sources" ] \
  || fail "case1: apt-get update ran before the prune (saw: $(tr '\n' ' ' < "${seen_at_update}"))"
pass "prunes non-Ubuntu entries before update, keeps ubuntu.sources, and names each removal"

# ---------------------------------------------------------------------
# Case 2: removes nothing when neither ubuntu.sources nor a non-empty
# sources.list is present — an EMPTY sources.list counts as absent too.
# ---------------------------------------------------------------------
sources_dir2="${work_dir}/case2/sources.list.d"
sources_list2="${work_dir}/case2/sources.list"
mkdir -p "${sources_dir2}"
: > "${sources_list2}"
: > "${sources_dir2}/some-ppa.list"

reset_apt_log
out2="$(CI_APT_SOURCES_DIR="${sources_dir2}" CI_APT_SOURCES_LIST="${sources_list2}" \
  "${INSTALL_SCRIPT}" libclang-dev 2>&1)"

[ -e "${sources_dir2}/some-ppa.list" ] || fail "case2: some-ppa.list was removed despite no Ubuntu archive being present"
printf '%s\n' "${out2}" | grep -qi "warning" || fail "case2: no warning printed when neither Ubuntu archive location is present"
assert_update_then_install libclang-dev
pass "removes nothing (and warns) when neither ubuntu.sources nor sources.list names the archive"

# ---------------------------------------------------------------------
# Case 2b: an empty ubuntu.sources and a comment-only sources.list (24.04's
# "moved" stub) are not an archive either, so nothing is removed.
# ---------------------------------------------------------------------
sources_dir2b="${work_dir}/case2b/sources.list.d"
sources_list2b="${work_dir}/case2b/sources.list"
mkdir -p "${sources_dir2b}"
: > "${sources_dir2b}/ubuntu.sources"
printf '# Ubuntu sources have moved to /etc/apt/sources.list.d/ubuntu.sources\n' > "${sources_list2b}"
: > "${sources_dir2b}/some-ppa.list"

reset_apt_log
CI_APT_SOURCES_DIR="${sources_dir2b}" CI_APT_SOURCES_LIST="${sources_list2b}" \
  "${INSTALL_SCRIPT}" libclang-dev > /dev/null 2>&1

[ -e "${sources_dir2b}/some-ppa.list" ] || fail "case2b: some-ppa.list was removed with no active Ubuntu archive"
assert_update_then_install libclang-dev
pass "an empty ubuntu.sources or a comment-only sources.list does not count as the archive"

# ---------------------------------------------------------------------
# Case 2c: a deb822 stanza marked `Enabled: no` is not an active archive,
# while a disabled stanza beside an enabled one still leaves the archive
# active.
# ---------------------------------------------------------------------
sources_dir2c="${work_dir}/case2c/sources.list.d"
sources_list2c="${work_dir}/case2c/sources.list"
mkdir -p "${sources_dir2c}"
printf 'Types: deb\nURIs: http://archive.ubuntu.com/ubuntu/\nSuites: noble\nComponents: main\nEnabled: no\n' \
  > "${sources_dir2c}/ubuntu.sources"
: > "${sources_list2c}"
: > "${sources_dir2c}/some-ppa.list"

reset_apt_log
CI_APT_SOURCES_DIR="${sources_dir2c}" CI_APT_SOURCES_LIST="${sources_list2c}" \
  "${INSTALL_SCRIPT}" libclang-dev > /dev/null 2>&1
[ -e "${sources_dir2c}/some-ppa.list" ] || fail "case2c: some-ppa.list was removed though the only Ubuntu stanza is disabled"

printf '\nTypes: deb\nURIs: http://security.ubuntu.com/ubuntu/\nSuites: noble-security\nComponents: main\n' \
  >> "${sources_dir2c}/ubuntu.sources"
reset_apt_log
CI_APT_SOURCES_DIR="${sources_dir2c}" CI_APT_SOURCES_LIST="${sources_list2c}" \
  "${INSTALL_SCRIPT}" libclang-dev > /dev/null
[ ! -e "${sources_dir2c}/some-ppa.list" ] || fail "case2c: some-ppa.list was kept though an enabled Ubuntu stanza remains"
assert_update_then_install libclang-dev
for disabled in 'Enabled:no' 'Enabled: 0' 'Enabled: without' $'Enabled: no\r'; do
  printf 'Types: deb\nURIs: http://archive.ubuntu.com/ubuntu/\nSuites: noble\nComponents: main\n%s\n' "${disabled}" \
    > "${sources_dir2c}/ubuntu.sources"
  : > "${sources_dir2c}/some-ppa.list"
  reset_apt_log
  CI_APT_SOURCES_DIR="${sources_dir2c}" CI_APT_SOURCES_LIST="${sources_list2c}" \
    "${INSTALL_SCRIPT}" libclang-dev > /dev/null 2>&1
  [ -e "${sources_dir2c}/some-ppa.list" ] || fail "case2c: '${disabled}' did not count as disabled"
done

printf 'Types: deb\nURIs:http://archive.ubuntu.com/ubuntu/\nSuites: noble\nComponents: main\n' \
  > "${sources_dir2c}/ubuntu.sources"
reset_apt_log
CI_APT_SOURCES_DIR="${sources_dir2c}" CI_APT_SOURCES_LIST="${sources_list2c}" \
  "${INSTALL_SCRIPT}" libclang-dev > /dev/null
[ ! -e "${sources_dir2c}/some-ppa.list" ] || fail "case2c: a URIs field with no space after the colon did not count"
pass "a stanza marked Enabled: no (any spelling apt reads as false) does not count as the archive; an enabled one does"

# ---------------------------------------------------------------------
# Case 3: prunes when only a non-empty sources.list is present
# (pre-24.04 layout, panel correction 22).
# ---------------------------------------------------------------------
sources_dir3="${work_dir}/case3/sources.list.d"
sources_list3="${work_dir}/case3/sources.list"
mkdir -p "${sources_dir3}"
printf 'deb http://archive.ubuntu.com/ubuntu jammy main\n' > "${sources_list3}"
: > "${sources_dir3}/some-ppa.list"

reset_apt_log
CI_APT_SOURCES_DIR="${sources_dir3}" CI_APT_SOURCES_LIST="${sources_list3}" \
  "${INSTALL_SCRIPT}" libclang-dev > /dev/null

[ ! -e "${sources_dir3}/some-ppa.list" ] || fail "case3: some-ppa.list was not pruned though sources.list is non-empty"
assert_update_then_install libclang-dev
pass "prunes when only sources.list names the archive"

# ---------------------------------------------------------------------
# Case 4: passes install args verbatim — an arg with a space, a
# --no-install-recommends-style flag, and multiple packages.
# ---------------------------------------------------------------------
sources_dir4="${work_dir}/case4/sources.list.d"
sources_list4="${work_dir}/case4/sources.list"
mkdir -p "${sources_dir4}"
write_ubuntu_sources "${sources_dir4}/ubuntu.sources"
: > "${sources_list4}"

reset_apt_log
CI_APT_SOURCES_DIR="${sources_dir4}" CI_APT_SOURCES_LIST="${sources_list4}" \
  "${INSTALL_SCRIPT}" --no-install-recommends "pkg with space" libclang-dev fonts-noto-cjk > /dev/null

assert_update_then_install --no-install-recommends "pkg with space" libclang-dev fonts-noto-cjk
pass "passes install args verbatim, including a spaced arg and a flag"

# ---------------------------------------------------------------------
# Case 5: update is invoked with -o Acquire::Retries=3 (checked as part of
# every assert_update_then_install call above, so this is a focused pin).
# ---------------------------------------------------------------------
grep -A2 '^update$' "${apt_log}" | grep -q -- '-o' || fail "case5: update call missing -o flag"
grep -A2 '^update$' "${apt_log}" | grep -q -- 'Acquire::Retries=3' || fail "case5: update call missing Acquire::Retries=3"
pass "apt-get update is invoked with -o Acquire::Retries=3"

# ---------------------------------------------------------------------
# Case 6: a failing apt-get update exits non-zero; install is never invoked.
# ---------------------------------------------------------------------
sources_dir6="${work_dir}/case6/sources.list.d"
sources_list6="${work_dir}/case6/sources.list"
mkdir -p "${sources_dir6}"
write_ubuntu_sources "${sources_dir6}/ubuntu.sources"
: > "${sources_list6}"

reset_apt_log
if CI_APT_SOURCES_DIR="${sources_dir6}" CI_APT_SOURCES_LIST="${sources_list6}" \
  FAKE_APT_FAIL_UPDATE=1 "${INSTALL_SCRIPT}" libclang-dev > /dev/null 2>"${work_dir}/case6.err"; then
  fail "case6: script exited 0 despite apt-get update failing"
fi
grep -q '^install$' "${apt_log}" && fail "case6: apt-get install was invoked despite update failing"
pass "a failing apt-get update exits non-zero and never invokes install"

# ---------------------------------------------------------------------
# Case 7: a second run in the same job is clean — no-op prune, then a
# normal update/install (idempotent).
# ---------------------------------------------------------------------
sources_dir7="${work_dir}/case7/sources.list.d"
sources_list7="${work_dir}/case7/sources.list"
mkdir -p "${sources_dir7}"
write_ubuntu_sources "${sources_dir7}/ubuntu.sources"
: > "${sources_dir7}/some-ppa.list"
: > "${sources_list7}"

reset_apt_log
CI_APT_SOURCES_DIR="${sources_dir7}" CI_APT_SOURCES_LIST="${sources_list7}" \
  "${INSTALL_SCRIPT}" libclang-dev > /dev/null
[ ! -e "${sources_dir7}/some-ppa.list" ] || fail "case7: first run did not prune some-ppa.list"

reset_apt_log
second_out="$(CI_APT_SOURCES_DIR="${sources_dir7}" CI_APT_SOURCES_LIST="${sources_list7}" \
  "${INSTALL_SCRIPT}" libclang-dev 2>&1)"
[ -e "${sources_dir7}/ubuntu.sources" ] || fail "case7: second run removed ubuntu.sources"
printf '%s\n' "${second_out}" | grep -q "some-ppa.list" && fail "case7: second run tried to remove some-ppa.list again"
assert_update_then_install libclang-dev
pass "a second run in the same job is a no-op prune followed by a normal update/install"

# ---------------------------------------------------------------------
# Case 8: a subdirectory under sources.list.d is not a source; it is left
# alone and does not stop the run.
# ---------------------------------------------------------------------
sources_dir8="${work_dir}/case8/sources.list.d"
sources_list8="${work_dir}/case8/sources.list"
mkdir -p "${sources_dir8}/nested"
write_ubuntu_sources "${sources_dir8}/ubuntu.sources"
: > "${sources_dir8}/some-ppa.list"
: > "${sources_list8}"

reset_apt_log
CI_APT_SOURCES_DIR="${sources_dir8}" CI_APT_SOURCES_LIST="${sources_list8}" \
  "${INSTALL_SCRIPT}" libclang-dev > /dev/null \
  || fail "case8: a subdirectory under sources.list.d stopped the run"
[ -d "${sources_dir8}/nested" ] || fail "case8: the subdirectory was removed"
[ ! -e "${sources_dir8}/some-ppa.list" ] || fail "case8: some-ppa.list was not pruned"
assert_update_then_install libclang-dev
pass "a subdirectory under sources.list.d is skipped"

echo "All ci-apt-install.sh tests passed."
