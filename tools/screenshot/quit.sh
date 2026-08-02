#!/usr/bin/env bash
# Cleanly quit a Roost UI (exercises the fsync-on-clean-exit path, so
# the next launch restores the persisted tab layout).
#
#   tools/screenshot/quit.sh mac
#   tools/screenshot/quit.sh gtk
#   tools/screenshot/quit.sh iced
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
ut_init "${1:?usage: quit.sh <mac|gtk|iced>}"
ut_quit
