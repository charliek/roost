#!/usr/bin/env bash
# Shared helpers for the linux/scripts/*.sh package checks. Sourced, not run.

# Print `error: <msg>` on stderr and bail. Inside GitHub Actions also emit the
# `::error::` workflow command so the failure surfaces as an annotation on the
# run; outside Actions that line is just noise, so it's gated.
die() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    printf '::error::%s\n' "$*"
  fi
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# Fail with one clear sentence naming the first missing tool.
require_tools() {
  local tool
  for tool in "$@"; do
    command -v "${tool}" >/dev/null 2>&1 \
      || die "required tool '${tool}' is not on PATH — install it and re-run."
  done
}

# Absolute path of an existing file, without assuming anything about cwd.
abspath() {
  local dir base
  dir="$(cd "$(dirname "$1")" && pwd)" || die "cannot resolve directory of '$1'"
  base="$(basename "$1")"
  printf '%s/%s\n' "${dir}" "${base}"
}
