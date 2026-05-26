#!/usr/bin/env bash
# Cleanly quit a Roost UI (exercises the fsync-on-clean-exit path, so
# the next launch restores the persisted tab layout).
#
#   tools/uitest/quit.sh mac
#   tools/uitest/quit.sh gtk
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
ut_init "${1:?usage: quit.sh <mac|gtk>}"
ut_quit
