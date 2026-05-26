#!/usr/bin/env bash
# Launch a Roost UI for testing and wait until its IPC socket answers.
#
#   tools/uitest/launch.sh mac     # open Roost.app (bundles it if missing)
#   tools/uitest/launch.sh gtk     # run target/debug/roost (Roost-gtk profile)
#
# Idempotent: a no-op if that UI is already running.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
ut_init "${1:?usage: launch.sh <mac|gtk>}"
ut_launch
