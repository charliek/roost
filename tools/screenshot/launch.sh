#!/usr/bin/env bash
# Launch a Roost UI for testing and wait until its IPC socket answers.
#
#   tools/screenshot/launch.sh mac  # open Roost.app (bundles if missing)
#   tools/screenshot/launch.sh gtk  # run target/debug/roost
#   tools/screenshot/launch.sh iced # run target/debug/roost-iced
#
# Idempotent: a no-op if that UI is already running.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
ut_init "${1:?usage: launch.sh <mac|gtk|iced>}"
ut_launch
