#!/usr/bin/env bash
# @roost.label: Fixture Provider
#
# Provider script for the E2E suite (discovered because it sits in the
# providers/ dir beside the seeded ROOST_CONFIG). Exercises the full
# contract deterministically with no external deps:
#
#   list      → two items (Provider Alpha / Provider Beta)
#   activate  → one item echoing $ROOST_SELECTED_ID, proving the chosen id
#               round-trips back through env (and the drill-in path works)
#
# Driven by test_provider.py.
set -euo pipefail

case "${1:-}" in
  list)
    printf '{"placeholder":"pick fixture","items":[{"id":"alpha","title":"Provider Alpha","subtitle":"first"},{"id":"beta","title":"Provider Beta"}]}'
    ;;
  activate)
    printf '{"items":[{"id":"done","title":"picked %s"}]}' "${ROOST_SELECTED_ID:-?}"
    ;;
  *)
    printf '{"items":[]}'
    ;;
esac
