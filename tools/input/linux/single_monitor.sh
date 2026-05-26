#!/usr/bin/env bash
# Drive a single enabled output for reliable absolute-pointer injection.
#
# A Wayland compositor binds a uinput absolute device to ONE output (usually
# the primary), so on a multi-monitor setup inject_pointer.py clicks aimed at
# a window on a different output silently miss. This collapses to a single
# enabled output for the duration of a test run, then restores the rest.
#
# Uses cosmic-randr (COSMIC desktop). Adapt for other compositors as needed.
#
#   single_monitor.sh status            # list outputs + enabled/disabled
#   single_monitor.sh solo <OUTPUT>     # disable every OTHER enabled output
#   single_monitor.sh restore           # re-enable what `solo` disabled
#
# `solo` records the outputs it disabled in $STATE so `restore` can undo it.
set -euo pipefail

STATE="${TMPDIR:-/tmp}/roost-single-monitor-disabled"

# Emit "<name> <enabled|disabled>" per output, ANSI stripped.
list_outputs() {
    cosmic-randr list 2>/dev/null \
        | sed -E 's/\x1b\[[0-9;]*m//g' \
        | grep -E '^\S+ \((enabled|disabled)\)' \
        | sed -E 's/^(\S+) \((enabled|disabled)\).*/\1 \2/'
}

case "${1:-}" in
status)
    list_outputs
    ;;
solo)
    keep="${2:?usage: single_monitor.sh solo <OUTPUT>}"
    : >"$STATE"
    while read -r name state; do
        if [[ "$name" != "$keep" && "$state" == "enabled" ]]; then
            echo "disabling $name"
            cosmic-randr disable "$name"
            echo "$name" >>"$STATE"
        fi
    done < <(list_outputs)
    echo "kept: $keep  (disabled list -> $STATE)"
    ;;
restore)
    if [[ ! -f "$STATE" ]]; then
        echo "no state file ($STATE); nothing to restore" >&2
        exit 0
    fi
    while read -r name; do
        [[ -z "$name" ]] && continue
        echo "enabling $name"
        cosmic-randr enable "$name"
    done <"$STATE"
    rm -f "$STATE"
    echo "restored; verify position/scale with: single_monitor.sh status"
    ;;
*)
    sed -n '2,18p' "$0"
    exit 1
    ;;
esac
