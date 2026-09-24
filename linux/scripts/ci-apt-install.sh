#!/usr/bin/env bash
# Install apt packages on a GitHub Actions runner through one path, so a
# preinstalled third-party source's 403 can't fail an unrelated job (#529).
#
# GitHub's runner images keep the Ubuntu archive itself in
# /etc/apt/sources.list.d/ubuntu.sources (24.04+) or, on the pre-24.04
# layout, in /etc/apt/sources.list. Anything else under sources.list.d is a
# preinstalled third-party runner source (a language toolchain PPA, a cloud
# CLI's own repo, ...) that this job never asked for and that can 403 or
# time out on its own schedule, independent of anything Roost's CI installs.
# Pruning those before `apt-get update` means a job that only ever wants
# Ubuntu packages can't be failed by a repo it doesn't use.
#
# Usage (from a workflow step, invoked with sudo since apt needs root):
#   sudo linux/scripts/ci-apt-install.sh <apt-get install args...>
#
# Everything after the script name is passed to `apt-get install -y`
# verbatim — same package list, same flags, in the same order a bare
# `apt-get install -y ...` would have used.
#
# CI_APT_SOURCES_DIR / CI_APT_SOURCES_LIST override the two paths this
# script prunes/inspects. They exist ONLY so ci-apt-install_test.sh can
# point this script at a scratch directory instead of the real
# /etc/apt/sources.list.d and /etc/apt/sources.list — a workflow invocation
# should never set them.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=linux/scripts/_common.sh
. "${SCRIPT_DIR}/_common.sh"

[ "$#" -ge 1 ] || die "usage: $(basename "$0") <apt-get install args...> — at least one package/flag is required"

sources_dir="${CI_APT_SOURCES_DIR:-/etc/apt/sources.list.d}"
sources_list="${CI_APT_SOURCES_LIST:-/etc/apt/sources.list}"

# Safety: prune only when the Ubuntu archive is an active source in one of
# the two layouts, so apt is never left with nothing to install from. An
# empty ubuntu.sources, a stanza marked `Enabled: no`, or a comment-only
# sources.list (24.04's own stub says the sources "have moved") does not
# count.
enabled_deb822_stanza() {
  [ -f "$1" ] || return 1
  awk '
    BEGIN { uris = 0; enabled = 1; found = 0 }
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*$/ { if (uris && enabled) found = 1; uris = 0; enabled = 1; next }
    /^[^[:space:]]/ {
      colon = index($0, ":")
      if (colon == 0) next
      key = tolower(substr($0, 1, colon - 1))
      value = tolower(substr($0, colon + 1))
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", key)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
      if (key == "uris") uris = 1
      if (key == "enabled" && value ~ /^(no|false|without|off|disable|0)$/) enabled = 0
    }
    END { if (uris && enabled) found = 1; exit !found }
  ' "$1"
}

ubuntu_archive_present=0
if enabled_deb822_stanza "${sources_dir}/ubuntu.sources"; then
  ubuntu_archive_present=1
elif grep -qsE '^[[:space:]]*deb[[:space:]]' "${sources_list}"; then
  ubuntu_archive_present=1
fi

if [ "${ubuntu_archive_present}" -eq 1 ]; then
  if [ -d "${sources_dir}" ]; then
    for entry in "${sources_dir}"/*; do
      [ -e "${entry}" ] || [ -L "${entry}" ] || continue
      # apt reads only files here; a subdirectory is not a source.
      if [ -d "${entry}" ] && [ ! -L "${entry}" ]; then
        continue
      fi
      name="$(basename "${entry}")"
      [ "${name}" = "ubuntu.sources" ] && continue
      printf 'ci-apt-install: removing third-party apt source %s\n' "${name}"
      rm -f -- "${entry}"
    done
  fi
else
  printf 'ci-apt-install: warning: no active Ubuntu archive in %s/ubuntu.sources or %s — leaving all apt sources untouched\n' \
    "${sources_dir}" "${sources_list}" >&2
fi

apt-get update -o Acquire::Retries=3
apt-get install -y "$@"
