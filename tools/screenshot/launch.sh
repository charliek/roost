#!/usr/bin/env bash
# Launch a Roost UI for testing and wait until its IPC socket answers.
#
#   tools/screenshot/launch.sh       # run target/debug/roost-iced (builds if missing)
#   tools/screenshot/launch.sh iced  # the same; `iced` is the only target
#
# Idempotent: a no-op if that UI is already running.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
ut_init "${1:-iced}"
ut_launch
