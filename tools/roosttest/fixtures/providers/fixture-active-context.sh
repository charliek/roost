#!/usr/bin/env bash
# @roost.label: Active Context Probe
#
# Provider script for plan 071 D14 (#533): echoes exactly what
# `provider_context` handed this run, so an E2E can tell a stale
# in-process reading (empty cwd, no ids) apart from the session slot's
# real active tab — and that the spawn actually landed `cwd` there
# rather than merely setting the env var.
#
# Driven by test_local_backend.py.
set -euo pipefail

case "${1:-}" in
  list)
    printf '{"items":[{"id":"cwd","title":"cwd:%s"},{"id":"tab","title":"tab:%s"},{"id":"pwd","title":"pwd:%s"}]}' \
      "${ROOST_ACTIVE_CWD:-}" "${ROOST_ACTIVE_TAB_ID:-}" "$(pwd)"
    ;;
  *)
    printf '{"items":[]}'
    ;;
esac
